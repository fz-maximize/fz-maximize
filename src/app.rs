//! The running application: hidden window, message loop, event handling.
//!
//! Everything runs on one thread. State lives in a thread-local `RefCell`; it is never
//! borrowed across a call that can pump messages (modal menus, cross-thread sends), and
//! a re-entrant access is dropped with a warning instead of panicking.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GA_ROOT, GetAncestor, GetCursorPos,
    GetForegroundWindow, GetMessageW, IsWindow, KillTimer, MSG, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SPI_SETWORKAREA, SW_MAXIMIZE, SW_RESTORE, SetTimer, ShowWindowAsync, TranslateMessage,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_SETTINGCHANGE,
    WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::animate::{self, Animator};
use crate::config::{self, BypassModifier, Config, MismatchAction};
use crate::drift::{self, DriftState, DriftTracker, Outcome, Record};
use crate::engine::{Decision, Engine, MaximizeInput, NativeReason, Observation, Options};
use crate::fancyzones::{self, FzStore, MonitorDesc, Resolution, desktop_candidates};
use crate::filters::{is_app_excluded, normalize_list};
use crate::geometry::{Margins, Rect};
use crate::hooks::{Hooks, WinEvent};
use crate::monitors::{LiveMonitor, MonitorCache};
use crate::preempt::{self, InputHooks, Kind, WM_APP_PREEMPT};
use crate::tray::{self, Command, MenuState, Tray};
use crate::win::{hwnd_from, hwnd_key, point_from};
use crate::{autostart, logging, policy, settings, vdesktop, window};

pub const WINDOW_CLASS: PCWSTR = w!("fz-maximize.hidden-window");
const WM_APP_TRAY: u32 = WM_APP + 1;
/// Posted by `fz-maximize --settings` to the running instance.
pub const WM_APP_OPEN_SETTINGS: u32 = WM_APP + 4;
const HOTKEY_ID: i32 = 1;
const PRUNE_THRESHOLD: usize = 2048;
const DRIFT_TIMER_ID: usize = 7;
/// Give FancyZones this long after a drop to place + stamp the window.
const DRIFT_SETTLE: Duration = Duration::from_millis(350);
/// Stop waiting for a FancyZones placement after this long.
const DRIFT_GIVE_UP: Duration = Duration::from_secs(2);

/// A move/size loop being watched for a FancyZones snap (drift check).
#[derive(Debug, Default)]
struct DragProbe {
    stamp_before: (u64, u64),
    /// Last window rect seen during the drag (the drop position).
    last_drag_rect: Option<Rect>,
    ended: Option<Instant>,
}

/// Identifies "this monitor with this layout" for drift bookkeeping.
fn drift_key(desc: &MonitorDesc, res: Option<&Resolution>) -> String {
    let layout = match res {
        Some(r) => format!("{}/{}z/{}px/{}", r.kind.as_str(), r.zone_count, r.spacing, r.uuid),
        None => "no-layout".into(),
    };
    format!("{}#{}|{layout}", desc.id.device.id, desc.id.device.instance_id)
}

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` with the app state. Returns `None` if there is no app or it is already
/// borrowed further up the stack (re-entrancy), in which case the event is dropped.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => guard.as_mut().map(f),
        Err(_) => {
            tracing::warn!("re-entrant event dropped");
            None
        }
    })
}

pub struct App {
    hwnd: HWND,
    exe: PathBuf,
    config_path: PathBuf,
    config: Config,
    user_excluded: Vec<String>,
    store: FzStore,
    monitors: MonitorCache,
    engine: Engine,
    tray: Option<Tray>,
    hotkey_active: bool,
    snaps: u64,
    drift: DriftTracker,
    drift_path: Option<PathBuf>,
    probes: HashMap<isize, DragProbe>,
    input_hooks: Option<InputHooks>,
    animator: Option<Animator>,
}

impl App {
    fn apply_config(&mut self, cfg: Config) {
        self.user_excluded = normalize_list(&cfg.excluded_apps);
        logging::set_level(&cfg.log_level);
        self.config = cfg;
        self.register_hotkey();
        self.sync_input_hooks();
    }

    /// Run the low-level input hooks only while an interception is enabled (they add a
    /// hop to every input event, so they shouldn't exist when unused).
    fn sync_input_hooks(&mut self) {
        let (mouse, win_up) = (self.config.preempt_mouse_maximize, self.config.preempt_win_up);
        preempt::configure(mouse, win_up, self.config.win_up_bypass, self.config.bypass_modifier);
        preempt::set_paused(self.engine.paused());
        let wanted = mouse || win_up;
        if wanted && self.input_hooks.is_none() {
            self.input_hooks = InputHooks::start(self.hwnd);
            if self.input_hooks.is_some() {
                tracing::info!(mouse, win_up, "input hooks installed (maximize flash prevention)");
            }
        } else if !wanted && self.input_hooks.take().is_some() {
            tracing::info!("input hooks removed");
        }
    }

    fn register_hotkey(&mut self) {
        if self.hotkey_active {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            }
            self.hotkey_active = false;
        }
        let Some(hk) = self.config.hotkey() else { return };
        let mods = HOT_KEY_MODIFIERS(hk.modifiers) | MOD_NOREPEAT;
        match unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ID, mods, hk.vk) } {
            Ok(()) => {
                self.hotkey_active = true;
                tracing::info!(hotkey = %self.config.hotkey, "hotkey registered");
            }
            Err(e) => {
                tracing::warn!(hotkey = %self.config.hotkey, error = %e, "could not register hotkey (in use by another app?)");
                if let Some(t) = &self.tray {
                    t.notify(
                        "fz-maximize",
                        &format!(
                            "Hotkey {} is already in use; full-maximize hotkey disabled.",
                            self.config.hotkey
                        ),
                        true,
                    );
                }
            }
        }
    }

    fn tooltip(&self) -> String {
        if self.engine.paused() {
            "fz-maximize (paused)".into()
        } else {
            "fz-maximize: maximize fills the FancyZones zone".into()
        }
    }

    fn refresh_tray(&mut self) {
        let tip = self.tooltip();
        let paused = self.engine.paused();
        if let Some(t) = &mut self.tray {
            t.set_state(paused, &tip);
        }
    }

    fn excluded_lists(&self, settings_excluded: &[String]) -> Vec<String> {
        let mut list: Vec<String> = window::DEFAULT_EXCLUDED_APPS.iter().map(|s| s.to_string()).collect();
        list.extend(self.user_excluded.iter().cloned());
        if self.config.respect_fancyzones_excluded_apps {
            list.extend(settings_excluded.iter().cloned());
        }
        list
    }

    /// A rising zoomed edge: Windows already maximized the window. Decide and correct.
    fn on_maximized(&mut self, hwnd: HWND) {
        let now = Instant::now();
        let key = hwnd_key(hwnd);
        if let Err(reason) = self
            .engine
            .precheck(key, now, modifier_held(self.config.bypass_modifier))
        {
            tracing::debug!(hwnd = key, %reason, "native maximize");
            return;
        }
        let Some(normal) = window::normal_rect_screen(hwnd) else {
            return;
        };
        self.redirect(hwnd, normal, true, false, "maximize (corrected after the fact)", now);
    }

    /// A gesture the input hooks intercepted before Windows maximized the window: it is
    /// still in its normal state, so placing it causes no maximize animation. Whenever
    /// we decide not to place it, perform the maximize the user asked for.
    fn on_intercepted(&mut self, hwnd: HWND, kind: Kind) {
        let now = Instant::now();
        let key = hwnd_key(hwnd);
        if kind == Kind::Bypassed {
            // Windows is about to maximize natively; make sure we leave it alone even if
            // the modifier is released before the maximize event reaches us.
            self.engine.arm_allow_native(key, now);
            tracing::debug!(hwnd = key, "bypass modifier held on maximize gesture; native maximize");
            return;
        }
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() || window::is_zoomed(hwnd) || window::is_iconic(hwnd) {
            return;
        }
        if kind == Kind::WinUpFull {
            // The bypass combination: the user explicitly asked for a normal maximize.
            tracing::info!(hwnd = key, gesture = kind.label(), "full-monitor maximize (bypass)");
            self.native_maximize(hwnd, now);
            return;
        }
        if let Err(reason) = self
            .engine
            .precheck_intercepted(modifier_held(self.config.bypass_modifier))
        {
            tracing::debug!(hwnd = key, gesture = kind.label(), %reason, "intercepted; maximizing natively");
            self.native_maximize(hwnd, now);
            return;
        }
        // The window is in its normal state, so its invisible borders can be measured exactly.
        let margins = window::measure_margins(hwnd);
        if self.engine.is_tracked(key) {
            if let Some(m) = margins {
                self.engine.update_margins(key, m);
            }
        } else {
            self.engine.seed(key, false, margins);
        }
        let placed = match window::window_rect(hwnd) {
            Some(normal) => self.redirect(hwnd, normal, false, true, kind.label(), now),
            None => false,
        };
        if placed {
            // The swallowed press would have activated the window.
            if kind == Kind::MaxButton {
                window::activate(hwnd);
            }
        } else {
            self.native_maximize(hwnd, now);
        }
    }

    /// Maximize exactly as the swallowed gesture would have, without redirecting it.
    fn native_maximize(&mut self, hwnd: HWND, now: Instant) {
        self.engine.arm_allow_native(hwnd_key(hwnd), now);
        window::post_sc_maximize(hwnd);
    }

    /// Move `hwnd` (normal state) into `target`, animated when `animate` is set, the
    /// setting is on, Windows animations are on, and the preview can cover the window.
    fn place(&mut self, hwnd: HWND, target: Rect, same_dpi: bool, animate: bool) -> windows::core::Result<()> {
        if animate
            && self.config.animate_maximize
            && animate::system_animations_enabled()
            && let Some(from) = window::window_rect(hwnd)
        {
            // Don't animate into a zone the window can't be moved to (elevated apps).
            if animate::should_animate(&from, &target) && window::can_place(hwnd) {
                if self.animator.is_none() {
                    self.animator = Animator::start();
                }
                if let Some(animator) = &self.animator {
                    // The animation thread moves the window at the right moment.
                    animator.animate(animate::Request {
                        target: hwnd_key(hwnd),
                        from,
                        to: target,
                        style: self.config.animation_style.resolve(),
                        duration: self.config.animation_duration(),
                        all_monitors_same_dpi: same_dpi,
                    });
                    return Ok(());
                }
            }
        }
        window::restore_to(hwnd, target, same_dpi)
    }

    /// Shared decision and placement. `normal` is the window rect to decide from (its
    /// pre-maximize rect). Returns true when the window was placed (zone or toggle).
    fn redirect(
        &mut self,
        hwnd: HWND,
        normal: Rect,
        consider_drag: bool,
        animate: bool,
        source: &str,
        now: Instant,
    ) -> bool {
        let key = hwnd_key(hwnd);
        self.store.refresh();
        let fz = self.store.data();

        if let Err(why) = window::check_eligible(hwnd, fz.settings.allow_child_window_snap) {
            tracing::debug!(hwnd = key, "native maximize: {}", NativeReason::NotEligible(why));
            return false;
        }
        let path = window::process_path(hwnd);
        let exe = exe_name(&path);
        if window::is_system_app(&path.to_uppercase()) {
            tracing::debug!(hwnd = key, exe, "native maximize: system app");
            return false;
        }
        let title = window::title(hwnd);
        let excluded = self.excluded_lists(&fz.settings.excluded_apps);
        if is_app_excluded(&path, &title, &excluded) {
            tracing::debug!(hwnd = key, exe, "native maximize: {}", NativeReason::ExcludedApp);
            return false;
        }

        let Some(mon) = self.monitors.for_window(hwnd) else {
            tracing::debug!(hwnd = key, "native maximize: monitor unknown");
            return false;
        };
        let window_vd = vdesktop::window_desktop(hwnd);
        let desktops = desktop_candidates(
            window_vd.as_deref(),
            vdesktop::registry_current().as_deref(),
            fz.last_used_vd,
        );
        let (resolution, verdict) = policy::monitor_zones(&self.config, &fz, &mon.desc, &desktops);
        let resolution = match (resolution, verdict) {
            (Some(r), Ok(())) => r,
            (_, Err(reason)) => {
                tracing::debug!(hwnd = key, exe, monitor = %mon.desc.id.device.id, %reason, "native maximize");
                return false;
            }
            (None, Ok(())) => return false,
        };
        if self.config.on_fancyzones_mismatch == MismatchAction::Native
            && self.drift.is_confirmed(&drift_key(&mon.desc, Some(&resolution)))
        {
            tracing::debug!(hwnd = key, exe, "native maximize: {}", NativeReason::FancyZonesDisagree);
            return false;
        }

        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let input = MaximizeInput {
            zones: &resolution.zones,
            normal_rect: normal,
            cursor: point_from(cursor),
            fallback_margins: window::estimate_margins(hwnd),
            consider_drag,
        };
        let opts = Options {
            already_in_zone: self.config.already_in_zone,
            snap_on_drag: self.config.snap_on_drag_maximize,
        };
        let same_dpi = self.monitors.all_same_dpi();

        match self.engine.decide(key, &input, opts, now) {
            Decision::Native(reason) => {
                tracing::debug!(hwnd = key, exe, %reason, "native maximize");
                false
            }
            Decision::Snap {
                zone_index,
                zone,
                target,
                pre_snap,
            } => match self.place(hwnd, target, same_dpi, animate) {
                Ok(()) => {
                    self.engine.commit_snap(key, zone, target, pre_snap, now);
                    self.snaps += 1;
                    tracing::info!(
                        hwnd = key,
                        exe,
                        zone = zone_index + 1,
                        of = resolution.zones.len(),
                        %zone,
                        monitor = %mon.desc.id.device.id,
                        source,
                        "maximize redirected to zone"
                    );
                    true
                }
                Err(e) => {
                    log_placement_error(key, &exe, &e);
                    false
                }
            },
            Decision::Restore { target } => match self.place(hwnd, target, same_dpi, animate) {
                Ok(()) => {
                    self.engine.commit_restore(key, now);
                    tracing::info!(hwnd = key, exe, %target, source, "restored to pre-snap size (toggle)");
                    true
                }
                Err(e) => {
                    log_placement_error(key, &exe, &e);
                    false
                }
            },
        }
    }

    /// First normal-state location change after a snap: fix up the invisible borders.
    fn verify(&mut self, hwnd: HWND, zone: Rect) {
        let key = hwnd_key(hwnd);
        let (Some(wr), Some(fr)) = (window::window_rect(hwnd), window::frame_rect(hwnd)) else {
            return;
        };
        let margins = Margins::between(wr, fr);
        if !margins.is_plausible() {
            return;
        }
        self.engine.update_margins(key, margins);
        if fr.approx_eq(&zone, 1) {
            tracing::trace!(hwnd = key, "snap verified");
            return;
        }
        let target = zone.grow(margins);
        match window::move_to(hwnd, target) {
            Ok(()) => {
                self.engine.commit_correction(key, target);
                tracing::debug!(hwnd = key, frame = %fr, %zone, ?margins, "corrected frame after snap");
            }
            Err(e) => tracing::debug!(hwnd = key, error = %e, "frame correction failed"),
        }
    }

    fn on_hotkey(&mut self) {
        let fg = unsafe { GetForegroundWindow() };
        if fg.is_invalid() {
            return;
        }
        let root = unsafe { GetAncestor(fg, GA_ROOT) };
        if !window::is_trackable(root) {
            return;
        }
        let key = hwnd_key(root);
        if window::is_zoomed(root) {
            unsafe {
                let _ = ShowWindowAsync(root, SW_RESTORE);
            }
            tracing::info!(hwnd = key, "hotkey: restore");
        } else {
            if !self.engine.is_tracked(key) {
                self.engine.seed(key, false, window::measure_margins(root));
            }
            self.engine.arm_allow_native(key, Instant::now());
            unsafe {
                let _ = ShowWindowAsync(root, SW_MAXIMIZE);
            }
            tracing::info!(hwnd = key, "hotkey: full-monitor maximize");
        }
    }

    fn reload(&mut self, announce: bool) {
        let cfg = match config::load(&self.config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "config error; keeping previous configuration");
                if let Some(t) = &self.tray {
                    t.notify("fz-maximize: config error", &e, true);
                }
                return;
            }
        };
        self.apply_config(cfg);
        self.store.invalidate();
        self.store.refresh();
        self.monitors.clear();
        vdesktop::reset();
        let summary = self.summary();
        tracing::info!(%summary, "reloaded");
        if announce && let Some(t) = &self.tray {
            t.notify("fz-maximize", &format!("Reloaded. {summary}"), false);
        }
    }

    /// One-line status: how many monitors currently redirect maximize.
    fn summary(&mut self) -> String {
        self.store.refresh();
        let fz = self.store.data();
        let desktops = desktop_candidates(None, vdesktop::registry_current().as_deref(), fz.last_used_vd);
        let monitors = self.monitors.all();
        let active = monitors
            .iter()
            .filter(|m| policy::monitor_zones(&self.config, &fz, &m.desc, &desktops).1.is_ok())
            .count();
        format!(
            "Zone maximize on {active} of {} monitor(s); {} snap(s) this session",
            monitors.len(),
            self.snaps
        )
    }

    fn menu_state(&mut self) -> MenuState {
        MenuState {
            paused: self.engine.paused(),
            autostart: autostart::is_enabled(&self.exe),
            status: if self.engine.paused() {
                "Paused".into()
            } else {
                self.summary()
            },
            check: if self.config.fancyzones_check {
                self.drift.state.summary()
            } else {
                "FancyZones agreement check: off".into()
            },
        }
    }

    fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Settings => self.open_settings(),
            Command::TogglePause => {
                let paused = !self.engine.paused();
                self.engine.set_paused(paused);
                preempt::set_paused(paused);
                tracing::info!(paused, "pause toggled");
                self.refresh_tray();
            }
            Command::Reload => self.reload(true),
            Command::OpenConfig => {
                if let Err(e) = config::ensure_exists(&self.config_path) {
                    tracing::error!(error = %e, "cannot create config file");
                    return;
                }
                spawn_detached("notepad.exe", self.config_path.as_os_str());
            }
            Command::OpenLogs => {
                if let Some(dir) = logging::log_dir() {
                    let _ = std::fs::create_dir_all(&dir);
                    spawn_detached("explorer.exe", dir.as_os_str());
                }
            }
            Command::ToggleAutostart => {
                let result = if autostart::is_enabled(&self.exe) {
                    autostart::disable()
                } else {
                    autostart::enable(&self.exe)
                };
                if let Err(e) = result {
                    tracing::error!(error = %e, "could not change autostart");
                }
            }
            Command::Exit => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
        }
    }

    // -----------------------------------------------------------------------------------
    // FancyZones agreement check
    // -----------------------------------------------------------------------------------

    fn probe_start(&mut self, hwnd: HWND) {
        if !self.config.fancyzones_check {
            return;
        }
        self.probes.insert(
            hwnd_key(hwnd),
            DragProbe {
                stamp_before: window::fancyzones_stamp(hwnd),
                last_drag_rect: window::window_rect(hwnd),
                ended: None,
            },
        );
    }

    /// During a drag, remember where the user is dragging the window (the drop
    /// position), so we can tell afterwards whether FancyZones moved it.
    fn probe_track(&mut self, hwnd: HWND) {
        if let Some(p) = self.probes.get_mut(&hwnd_key(hwnd))
            && p.ended.is_none()
        {
            p.last_drag_rect = window::window_rect(hwnd);
        }
    }

    fn probe_end(&mut self, hwnd: HWND) {
        if let Some(p) = self.probes.get_mut(&hwnd_key(hwnd)) {
            p.ended = Some(Instant::now());
            self.arm_drift_timer(DRIFT_SETTLE);
        }
    }

    /// One-shot timer: only armed after a drag ends, never in steady state.
    fn arm_drift_timer(&self, after: Duration) {
        unsafe {
            SetTimer(Some(self.hwnd), DRIFT_TIMER_ID, after.as_millis() as u32, None);
        }
    }

    /// Evaluate drags whose drop has settled.
    fn process_probes(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), DRIFT_TIMER_ID);
        }
        let now = Instant::now();
        let mut ready = Vec::new();
        let mut waiting = false;
        self.probes.retain(|&key, p| {
            let Some(ended) = p.ended else { return true };
            let elapsed = now.duration_since(ended);
            if elapsed < DRIFT_SETTLE {
                waiting = true;
                return true;
            }
            let h = hwnd_from(key);
            if !unsafe { IsWindow(Some(h)) }.as_bool() {
                return false;
            }
            let stamp = window::fancyzones_stamp(h);
            if stamp == (0, 0) {
                // Not snapped by FancyZones (plain move, or dragged out of a zone).
                if elapsed < DRIFT_GIVE_UP {
                    waiting = true;
                    return true;
                }
                return false;
            }
            let moved = match (p.last_drag_rect, window::window_rect(h)) {
                (Some(drop), Some(now_rect)) => !drop.approx_eq(&now_rect, 1),
                _ => false,
            };
            if moved || stamp != p.stamp_before {
                ready.push((h, drift::decode_stamp(stamp.0, stamp.1)));
                return false;
            }
            // Stamped but not (yet) moved: FancyZones may still be applying the placement,
            // or this was a resize of an already-zoned window (not a snap). Wait, then drop.
            if elapsed < DRIFT_GIVE_UP {
                waiting = true;
                return true;
            }
            false
        });
        for (h, zones) in ready {
            if window::is_zoomed(h) || window::is_iconic(h) || zones.is_empty() {
                continue;
            }
            self.fancyzones_check(h, zones);
        }
        if waiting {
            self.arm_drift_timer(Duration::from_millis(250));
        }
    }

    /// Compare FancyZones' placement of `hwnd` in `zones` with our own computation.
    fn fancyzones_check(&mut self, hwnd: HWND, zones: Vec<usize>) {
        let Some(actual) = window::frame_rect(hwnd) else {
            return;
        };
        self.store.refresh();
        let fz = self.store.data();
        self.drift
            .set_powertoys_version(fz.powertoys_version.unwrap_or("unknown"));
        let Some(mon) = self.monitors.for_window(hwnd) else {
            return;
        };
        let desktops = desktop_candidates(
            vdesktop::window_desktop(hwnd).as_deref(),
            vdesktop::registry_current().as_deref(),
            fz.last_used_vd,
        );
        let res = fancyzones::resolve(&fz, &mon.desc, &desktops, self.config.use_fancyzones_default_layout);
        let outcome = drift::evaluate(res.as_ref().map(|r| r.zones.as_slice()), &zones, actual);
        let key = drift_key(&mon.desc, res.as_ref());
        let exe = exe_name(&window::process_path(hwnd));
        let zones_1based: Vec<usize> = zones.iter().map(|z| z + 1).collect();

        match &outcome {
            Outcome::Match { .. } => {
                tracing::info!(exe, zones = ?zones_1based, monitor = %mon.desc.id.device.id, "FancyZones agreement check: match")
            }
            o if o.is_drift() => {
                tracing::warn!(exe, zones = ?zones_1based, monitor = %mon.desc.id.device.id, key, "FancyZones agreement check: {}", o.describe())
            }
            o => tracing::info!(exe, zones = ?zones_1based, "FancyZones agreement check: {}", o.describe()),
        }

        let record = Record {
            at_unix: drift::now_unix(),
            exe,
            monitor: format!("{} ({})", mon.desc.id.device.id, mon.desc.device_name),
            layout: key.split('|').nth(1).unwrap_or_default().to_string(),
            zones: zones_1based,
            outcome: None,
        };
        let alert = self.drift.record(&key, (hwnd_key(hwnd), zones), record, outcome);
        if let Some(path) = &self.drift_path
            && let Err(e) = self.drift.state.save(path)
        {
            tracing::warn!(error = %e, "could not save FancyZones check state");
        }

        if let Some(alert) = alert {
            let version = fz.powertoys_version.unwrap_or("unknown");
            let action = match self.config.on_fancyzones_mismatch {
                MismatchAction::Warn => "Maximize still goes to fz-maximize's zones; they may be misaligned.",
                MismatchAction::Native => {
                    "Zone-maximize is now OFF on this monitor until PowerToys or the layout changes."
                }
            };
            tracing::error!(
                key = alert.key,
                detail = alert.detail,
                powertoys = version,
                "confirmed disagreement with FancyZones"
            );
            if let Some(t) = &self.tray {
                t.notify(
                    "fz-maximize disagrees with FancyZones",
                    &format!(
                        "PowerToys {version} places windows differently from fz-maximize on {}. {action} Run `fz-maximize --dump` for details.",
                        mon.desc.id.device.id
                    ),
                    true,
                );
            }
        }
    }

    fn open_settings(&mut self) {
        tracing::info!("opening settings window");
        let monitors = self.monitors.all();
        let monitors_hint = if monitors.is_empty() {
            String::new()
        } else {
            format!(
                "Connected: {}",
                monitors
                    .iter()
                    .map(|m| format!(
                        "{} ({}, {}x{})",
                        m.desc.id.device.id,
                        m.desc.device_name,
                        m.desc.rc_monitor.width(),
                        m.desc.rc_monitor.height()
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        settings::open(settings::Context {
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            autostart: autostart::is_enabled(&self.exe),
            monitors_hint,
            check_status: self.drift.state.summary(),
        });
    }

    /// Save settings from the settings window and apply them live.
    fn apply_settings(&mut self, cfg: Config, autostart_on: bool) -> Result<(), String> {
        config::save(&self.config_path, &cfg)?;
        self.apply_config(cfg);
        self.store.invalidate();
        self.store.refresh();
        self.monitors.clear();
        if autostart_on != autostart::is_enabled(&self.exe) {
            let r = if autostart_on {
                autostart::enable(&self.exe)
            } else {
                autostart::disable()
            };
            r.map_err(|e| format!("Settings saved, but Start with Windows could not be changed: {e}"))?;
        }
        self.refresh_tray();
        tracing::info!(path = %self.config_path.display(), "settings saved and applied");
        Ok(())
    }

    fn maybe_prune(&mut self) {
        if self.engine.tracked_windows() > PRUNE_THRESHOLD {
            let removed = self.engine.prune(|h| unsafe { IsWindow(Some(hwnd_from(h))) }.as_bool());
            tracing::debug!(removed, "pruned stale window state");
        }
    }

    /// Seed state for every existing top-level window so pre-existing maximized windows
    /// are not mistaken for new maximizes.
    fn seed_existing_windows(&mut self) {
        unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the &mut Vec below, alive for the enumeration.
            let v = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
            v.push(hwnd);
            BOOL(1)
        }
        let mut all: Vec<HWND> = Vec::new();
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
        }
        let mut seeded = 0;
        for hwnd in all.into_iter().filter(|h| window::is_trackable(*h)) {
            self.engine.seed(
                hwnd_key(hwnd),
                window::zoomed_for_seed(hwnd),
                window::measure_margins(hwnd),
            );
            seeded += 1;
        }
        tracing::info!(seeded, "existing windows seeded");
    }
}

/// Settings-window handler (called from the settings window's message handling, when
/// no app borrow is held).
fn apply_settings(cfg: Config, autostart_on: bool) -> Result<(), String> {
    with_app(|a| a.apply_settings(cfg, autostart_on)).unwrap_or_else(|| Err("fz-maximize is busy; try again.".into()))
}

fn log_placement_error(key: isize, exe: &str, e: &windows::core::Error) {
    // E_ACCESSDENIED: UIPI blocks moving windows of elevated processes.
    if e.code() == windows::Win32::Foundation::E_ACCESSDENIED {
        tracing::debug!(
            hwnd = key,
            exe,
            "cannot move window (elevated process?); leaving native maximize"
        );
    } else {
        tracing::warn!(hwnd = key, exe, error = %e, "SetWindowPlacement failed; leaving native maximize");
    }
}

/// `%LOCALAPPDATA%z-maximizeancyzones-check.json`
pub fn drift_state_path() -> Option<PathBuf> {
    logging::log_dir().map(|d| d.join("fancyzones-check.json"))
}

fn exe_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn spawn_detached(program: &str, arg: &std::ffi::OsStr) {
    if let Err(e) = std::process::Command::new(program).arg(arg).spawn() {
        tracing::error!(program, error = %e, "failed to launch");
    }
}

fn modifier_held(m: BypassModifier) -> bool {
    let vk = match m {
        BypassModifier::Shift => VK_SHIFT,
        BypassModifier::Ctrl => VK_CONTROL,
        BypassModifier::Alt => VK_MENU,
        BypassModifier::None => return false,
    };
    (unsafe { GetAsyncKeyState(i32::from(vk.0)) } as u16 & 0x8000) != 0
}

/// WinEvent hook handler.
fn on_win_event(ev: WinEvent, hwnd: HWND) {
    let key = hwnd_key(hwnd);
    match ev {
        WinEvent::Destroy => {
            with_app(|a| a.engine.forget(key));
        }
        WinEvent::Show => {
            if window::is_trackable(hwnd) {
                let zoomed = window::zoomed_for_seed(hwnd);
                let margins = window::measure_margins(hwnd);
                with_app(|a| {
                    a.engine.seed(key, zoomed, margins);
                    a.maybe_prune();
                });
            }
        }
        WinEvent::MoveSizeStart => {
            if window::is_trackable(hwnd) {
                with_app(|a| {
                    a.engine.move_size_start(key);
                    a.probe_start(hwnd);
                });
            }
        }
        WinEvent::MoveSizeEnd => {
            let margins = window::measure_margins(hwnd);
            with_app(|a| {
                a.engine.move_size_end(key, Instant::now());
                a.probe_end(hwnd);
                if let Some(m) = margins {
                    a.engine.update_margins(key, m);
                }
            });
        }
        WinEvent::LocationChange => {
            if !window::is_trackable(hwnd) || window::is_iconic(hwnd) {
                return;
            }
            let zoomed = window::is_zoomed(hwnd);
            let obs = with_app(|a| {
                if !a.probes.is_empty() {
                    a.probe_track(hwnd);
                }
                a.engine.observe(key, zoomed, Instant::now())
            });
            match obs {
                Some(Observation::Maximized) => {
                    with_app(|a| a.on_maximized(hwnd));
                }
                Some(Observation::Verify(zone)) => {
                    with_app(|a| a.verify(hwnd, zone));
                }
                _ => {}
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY if wparam.0 == HOTKEY_ID as usize => {
            with_app(|a| a.on_hotkey());
            LRESULT(0)
        }
        WM_APP_TRAY => {
            // NOTIFYICON_VERSION_4: LOWORD(lParam) is the event.
            match (lparam.0 as u32) & 0xFFFF {
                WM_CONTEXTMENU => {
                    // Gather state, then release the borrow before the modal menu loop.
                    if let Some(state) = with_app(|a| a.menu_state())
                        && let Some(cmd) = tray::show_menu(hwnd, &state)
                    {
                        with_app(|a| a.run_command(cmd));
                    }
                }
                WM_LBUTTONDBLCLK => {
                    with_app(|a| a.run_command(Command::TogglePause));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_OPEN_SETTINGS => {
            with_app(|a| a.open_settings());
            LRESULT(0)
        }
        WM_APP_PREEMPT => {
            if let Some(kind) = Kind::from_raw(lparam.0) {
                with_app(|a| a.on_intercepted(hwnd_from(wparam.0 as isize), kind));
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == DRIFT_TIMER_ID => {
            with_app(|a| a.process_probes());
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            tracing::info!("display configuration changed");
            with_app(|a| a.monitors.clear());
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            if wparam.0 == SPI_SETWORKAREA.0 as usize {
                tracing::debug!("work area changed");
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            tracing::info!("taskbar re-created (Explorer restarted); re-adding tray icon");
            vdesktop::reset();
            with_app(|a| {
                if let Some(t) = &a.tray {
                    t.add();
                }
                a.refresh_tray();
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn create_hidden_window() -> Result<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread()).context("RegisterClassExW");
        }
        // A real (hidden) top-level window, not HWND_MESSAGE: message-only windows don't
        // receive broadcasts such as TaskbarCreated / WM_DISPLAYCHANGE.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            WINDOW_CLASS,
            w!("fz-maximize"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .context("CreateWindowExW")?;
        Ok(hwnd)
    }
}

/// Run the tray application until exit.
pub fn run(config_path: PathBuf, cfg: Config, config_error: Option<String>) -> Result<()> {
    let hwnd = create_hidden_window()?;
    TASKBAR_CREATED.store(
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        Ordering::Relaxed,
    );

    let powertoys = fancyzones::powertoys_dir().context("LOCALAPPDATA is not set")?;
    let mut store = FzStore::new(&powertoys);
    store.refresh();
    if !store.fancyzones_dir().exists() {
        tracing::warn!(dir = %store.fancyzones_dir().display(), "FancyZones data folder not found; is PowerToys installed?");
    }

    let exe = std::env::current_exe().unwrap_or_default();
    let drift_path = drift_state_path();
    let app = App {
        hwnd,
        exe,
        config_path,
        config: Config::default(),
        user_excluded: Vec::new(),
        store,
        monitors: MonitorCache::default(),
        engine: Engine::new(),
        tray: None,
        hotkey_active: false,
        snaps: 0,
        drift: DriftTracker::new(drift_path.as_deref().map(DriftState::load).unwrap_or_default()),
        drift_path,
        probes: HashMap::new(),
        input_hooks: None,
        animator: None,
    };
    APP.with(|cell| *cell.borrow_mut() = Some(app));

    with_app(|a| {
        a.tray = Some(Tray::new(hwnd, WM_APP_TRAY));
        a.apply_config(cfg);
        a.refresh_tray();
        if let (Some(e), Some(t)) = (&config_error, &a.tray) {
            t.notify("fz-maximize: config error (using defaults)", e, true);
        }
        a.seed_existing_windows();
        let summary = a.summary();
        tracing::info!(%summary, "started");
    });

    settings::set_apply_handler(apply_settings);
    let hooks = Hooks::install(on_win_event).context("SetWinEventHook")?;

    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        if settings::dialog_message(&msg) {
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    drop(hooks);
    APP.with(|cell| {
        if let Some(mut app) = cell.borrow_mut().take() {
            if app.hotkey_active {
                unsafe {
                    let _ = UnregisterHotKey(Some(app.hwnd), HOTKEY_ID);
                }
            }
            app.input_hooks = None;
            app.animator = None;
            app.tray = None;
            tracing::info!(snaps = app.snaps, "exiting");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------
// --dump
// ---------------------------------------------------------------------------------------

#[derive(Serialize)]
struct DumpMonitor {
    device_name: String,
    id: crate::fancyzones::matching::MonitorId,
    primary: bool,
    rc_monitor: Rect,
    rc_work: Rect,
    dpi: u32,
    layout: Option<Resolution>,
    intercept: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct SnappedWindow {
    exe: String,
    title: String,
    /// 1-based zone numbers FancyZones stamped on the window.
    zones: Vec<usize>,
    monitor: String,
    outcome: Outcome,
}

#[derive(Serialize)]
struct Dump {
    version: &'static str,
    powertoys_version: Option<String>,
    fancyzones_dir: String,
    fancyzones_enabled: bool,
    config_path: String,
    config_error: Option<String>,
    desktops: Vec<String>,
    monitors: Vec<DumpMonitor>,
    /// History of FancyZones agreement checks made by the running app.
    fancyzones_check: DriftState,
    /// Windows FancyZones has snapped right now, compared with our zones.
    snapped_windows: Vec<SnappedWindow>,
}

/// Compare every window FancyZones currently has stamped with our zones. A window
/// resized after it was snapped shows up as a difference; that is expected.
fn audit_snapped_windows(
    fz: &fancyzones::FzView<'_>,
    cache: &mut MonitorCache,
    use_default: bool,
) -> Vec<SnappedWindow> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: `data` is the &mut Vec below, alive for the enumeration.
        let v = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        v.push(hwnd);
        BOOL(1)
    }
    let mut all: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut all as *mut _ as isize));
    }
    let mut out = Vec::new();
    for h in all {
        if !window::is_trackable(h) || window::is_zoomed(h) || window::is_iconic(h) || window::is_cloaked(h) {
            continue;
        }
        let stamp = window::fancyzones_stamp(h);
        if stamp == (0, 0) {
            continue;
        }
        let (Some(actual), Some(mon)) = (window::frame_rect(h), cache.for_window(h)) else {
            continue;
        };
        let zones = drift::decode_stamp(stamp.0, stamp.1);
        let desktops = desktop_candidates(
            vdesktop::window_desktop(h).as_deref(),
            vdesktop::registry_current().as_deref(),
            fz.last_used_vd,
        );
        let res = fancyzones::resolve(fz, &mon.desc, &desktops, use_default);
        out.push(SnappedWindow {
            exe: exe_name(&window::process_path(h)),
            title: window::title(h),
            zones: zones.iter().map(|z| z + 1).collect(),
            monitor: mon.desc.id.device.id.clone(),
            outcome: drift::evaluate(res.as_ref().map(|r| r.zones.as_slice()), &zones, actual),
        });
    }
    out
}

/// Produce the `--dump` report (text or JSON).
pub fn dump(config_path: &Path, json: bool) -> Result<String> {
    let (cfg, config_error) = match config::load(config_path) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let powertoys = fancyzones::powertoys_dir().context("LOCALAPPDATA is not set")?;
    let mut store = FzStore::new(&powertoys);
    store.refresh();
    let fz = store.data();
    let desktops = desktop_candidates(None, vdesktop::registry_current().as_deref(), fz.last_used_vd);
    let mut cache = MonitorCache::default();
    let monitors: Vec<LiveMonitor> = cache.all();

    let snapped_windows = audit_snapped_windows(&fz, &mut cache, cfg.use_fancyzones_default_layout);
    let report = Dump {
        version: env!("CARGO_PKG_VERSION"),
        powertoys_version: fz.powertoys_version.map(str::to_string),
        fancyzones_check: drift_state_path().as_deref().map(DriftState::load).unwrap_or_default(),
        snapped_windows,
        fancyzones_dir: store.fancyzones_dir().display().to_string(),
        fancyzones_enabled: fz.enabled,
        config_path: config_path.display().to_string(),
        config_error,
        desktops: desktops.clone(),
        monitors: monitors
            .iter()
            .map(|m| {
                let (layout, verdict) = policy::monitor_zones(&cfg, &fz, &m.desc, &desktops);
                DumpMonitor {
                    device_name: m.desc.device_name.clone(),
                    id: m.desc.id.clone(),
                    primary: m.primary,
                    rc_monitor: m.desc.rc_monitor,
                    rc_work: m.desc.rc_work,
                    dpi: m.desc.dpi,
                    layout,
                    intercept: verdict.is_ok(),
                    reason: verdict.err().map(|r| r.to_string()),
                }
            })
            .collect(),
    };

    if json {
        return Ok(serde_json::to_string_pretty(&report)?);
    }

    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "fz-maximize {}", report.version);
    let _ = writeln!(
        out,
        "FancyZones data: {} (module {})",
        report.fancyzones_dir,
        if report.fancyzones_enabled {
            "enabled"
        } else {
            "DISABLED"
        }
    );
    let _ = writeln!(out, "Config: {}", report.config_path);
    if let Some(e) = &report.config_error {
        let _ = writeln!(out, "  CONFIG ERROR (defaults in use): {e}");
    }
    let _ = writeln!(out, "Virtual desktop candidates: {}", report.desktops.join(", "));
    for m in &report.monitors {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{}{}  {} / {} / #{} / serial '{}'",
            m.device_name,
            if m.primary { " (primary)" } else { "" },
            m.id.device.id,
            m.id.device.instance_id,
            m.id.device.number,
            m.id.serial
        );
        let _ = writeln!(out, "  monitor {}  work {}  dpi {}", m.rc_monitor, m.rc_work, m.dpi);
        match &m.layout {
            Some(l) => {
                let name = l
                    .custom_name
                    .as_deref()
                    .map(|n| format!(" \"{n}\""))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "  layout {}{} ({} zones, spacing {}) from {}",
                    l.kind.as_str(),
                    name,
                    l.zone_count,
                    l.spacing,
                    match &l.source {
                        fancyzones::LayoutSource::Applied {
                            virtual_desktop,
                            quality,
                        } => format!("applied-layouts.json [{virtual_desktop}, {quality:?}]"),
                        fancyzones::LayoutSource::Default => "FancyZones default layout".into(),
                    }
                );
                if let Some(e) = &l.error {
                    let _ = writeln!(out, "  LAYOUT ERROR: {e}");
                }
                for (i, z) in l.zones.iter().enumerate() {
                    let _ = writeln!(out, "    zone {}: {z}", i + 1);
                }
            }
            None => {
                let _ = writeln!(out, "  no layout");
            }
        }
        let _ = writeln!(
            out,
            "  => {}",
            if m.intercept {
                "maximize goes to zone".to_string()
            } else {
                format!("native maximize ({})", m.reason.as_deref().unwrap_or("?"))
            }
        );
    }

    let c = &report.fancyzones_check.current;
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "FancyZones agreement check (installed PowerToys {}):",
        report.powertoys_version.as_deref().unwrap_or("unknown")
    );
    if c.checks == 0 {
        let _ = writeln!(
            out,
            "  no checks recorded yet: Shift-drag a window into a zone while fz-maximize runs"
        );
    } else {
        let _ = writeln!(
            out,
            "  {} check(s) on PowerToys {}: {} match, {} mismatch, {} inconclusive; last check {}, last match {}",
            c.checks,
            c.powertoys_version,
            c.matches,
            c.mismatches,
            c.inconclusive,
            drift::format_unix(c.last_check_unix),
            drift::format_unix(c.last_match_unix)
        );
    }
    if report
        .powertoys_version
        .as_deref()
        .is_some_and(|v| c.checks > 0 && v != c.powertoys_version)
    {
        let _ = writeln!(
            out,
            "  NOTE: PowerToys was updated since the last check; nothing verified on this version yet"
        );
    }
    if let Some(p) = &report.fancyzones_check.previous {
        let _ = writeln!(
            out,
            "  previous: PowerToys {}: {}/{} match",
            p.powertoys_version,
            p.matches,
            p.matches + p.mismatches
        );
    }
    for k in &report.fancyzones_check.confirmed {
        let _ = writeln!(out, "  CONFIRMED DRIFT: {k}");
    }
    for r in report.fancyzones_check.recent.iter().take(5) {
        let _ = writeln!(
            out,
            "  {}  {} zone {:?} on {}: {}",
            drift::format_unix(r.at_unix),
            r.exe,
            r.zones,
            r.monitor,
            r.outcome.as_ref().map(Outcome::describe).unwrap_or_default()
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Windows currently snapped by FancyZones ({}):",
        report.snapped_windows.len()
    );
    for w in &report.snapped_windows {
        let note = match &w.outcome {
            Outcome::Match { .. } => "matches fz-maximize's zone".to_string(),
            o => format!("{} (fine if the window was resized after snapping)", o.describe()),
        };
        let title: String = w.title.chars().take(40).collect();
        let _ = writeln!(
            out,
            "  {} \"{}\" zone {:?} on {}: {}",
            w.exe, title, w.zones, w.monitor, note
        );
    }
    Ok(out)
}

/// Ask a running instance to open its settings window. Returns true if one was found.
pub fn signal_open_settings() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, FindWindowW, PostMessageW};
    match unsafe { FindWindowW(WINDOW_CLASS, PCWSTR::null()) } {
        Ok(h) if !h.is_invalid() => {
            let mut pid = 0u32;
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(h, Some(&mut pid));
                // We are (usually) the foreground process; let the instance take focus.
                let _ = AllowSetForegroundWindow(pid);
                let _ = PostMessageW(Some(h), WM_APP_OPEN_SETTINGS, WPARAM(0), LPARAM(0));
            }
            true
        }
        _ => false,
    }
}

/// Ask a running instance to exit. Returns true if one was found.
pub fn signal_quit() -> bool {
    match unsafe { windows::Win32::UI::WindowsAndMessaging::FindWindowW(WINDOW_CLASS, PCWSTR::null()) } {
        Ok(h) if !h.is_invalid() => {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            true
        }
        _ => false,
    }
}
