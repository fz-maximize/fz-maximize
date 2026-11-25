# fz-maximize — Implementation Plan (revised)

This is the revised plan that the code in this repository implements. It starts from the
original proposal (kept verbatim in [`ORIGINAL_PLAN.md`](ORIGINAL_PLAN.md)), keeps its
goals and constraints, and changes the mechanics where the proposal would not have
worked against a real FancyZones install or would have had user-visible bugs.

All "FancyZones does X" statements below were checked against the PowerToys source
(`src/modules/fancyzones/FancyZonesLib`, `main` branch, PowerToys v0.102) and against
the FancyZones data on the development machine
(`%LOCALAPPDATA%\Microsoft\PowerToys\FancyZones\`).

---

## 1. Objective (unchanged)

A small, out-of-process Rust background utility. When a window is maximized
(maximize button, title-bar double-click, `Win`+`Up`, drag to the top edge), it is placed
into the FancyZones zone it mostly occupies instead of covering the whole monitor.

* **Bypass:** holding `Shift` while maximizing with the mouse, or the global hotkey
  (default `Ctrl`+`Alt`+`Up`), gives the native full-monitor maximize.
* **Per-monitor behavior:** taken from FancyZones. Monitors with no layout,
  a blank layout or a single zone are left alone.
* **Constraints:** no DLL injection, no global in-context hooks, no admin rights, no
  polling, no background threads in steady state.

## 2. Evaluation of the original plan: where this plan deviates

| # | Original plan | Problem found | Revised approach |
|---|---------------|---------------|------------------|
| D1 | Built-in templates read from `layout-templates.json` | That file only stores editor defaults (zone count, spacing); it has **no geometry**. On the dev machine it doesn't exist, and every applied layout is the built-in `priority-grid`. | Port FancyZones' `LayoutConfigurator` (blank, focus, columns, rows, grid, priority-grid, custom grid, custom canvas) to exact integer arithmetic so the zones match FancyZones to the pixel. `layout-templates.json` isn't needed. |
| D2 | "Match each HMONITOR to its entry in applied-layouts.json" (no method given) | Entries are keyed by PnP id (`SAM73CE`), device instance (`5&1498d1d&0&UID4352`), EDID serial, monitor number **and virtual-desktop GUID**. `\\.\DISPLAYn` is only used by legacy entries. | Port FancyZones' identification (`EnumDisplayDevicesW(.., EDD_GET_DEVICE_INTERFACE_NAME)`, number from `\\.\DISPLAYn`) and its exact `MonitorId`/`DeviceId` equality rules. The serial comes from the EDID in the registry instead of WMI: lighter, no COM/WMI, and the same value. |
| D3 | Virtual desktops ignored | Every applied-layout entry is per-virtual-desktop, so without the desktop id nothing matches reliably. | `IVirtualDesktopManager::GetWindowDesktopId(hwnd)` (documented API). Fallbacks: the registry current desktop (global key, then the per-session key that Win10 uses), then `last-used-virtual-desktop.json`, then `GUID_NULL` entries. |
| D4 | On `LOCATIONCHANGE`, if `IsZoomed` then snap | That re-snaps windows the user deliberately full-maximized (Shift) on **any** later location change, windows that were already maximized when the tool started, and windows restored from minimize. | **Edge-triggered.** Track the last observed zoomed state per HWND, act only on a not-zoomed → zoomed transition, seed the state for existing windows at startup and on `EVENT_OBJECT_SHOW`, and freeze it while the window is minimized. |
| D5 | Cache `last_normal_rect` on every location change | Duplicates state Windows already keeps, does work on every move/resize event, and is wrong for windows never seen in the normal state. | Read `GetWindowPlacement().rcNormalPosition` at maximize time. That is the authoritative pre-maximize rect. |
| D6 | `ShowWindow(SW_RESTORE)` then `SetWindowPos` | Two visible transitions (restore to the old size, then a jump), blocks on hung windows, and leaves the window's restore position pointing at the old rect. | A single `SetWindowPlacement` with `showCmd = SW_RESTORE`, `rcNormalPosition = zone` (converted to **workspace coordinates**) and `WPF_ASYNCWINDOWPLACEMENT`. This is what FancyZones' own `SizeWindowToRect` does. |
| D7 | Measure invisible borders with `GetWindowRect` vs `DWMWA_EXTENDED_FRAME_BOUNDS` at snap time | At snap time the window is **maximized**, and maximized windows have different frame geometry (all four sides hang off-screen), so the margins would be wrong. | Cache the margins whenever the window is seen in the normal state (startup seed, show, move/size end). After the restore, a **one-shot verify** measures the real frame and corrects with `SetWindowPos` if it is off by more than 1 px (this also covers DPI changes and apps with custom frames). |
| D8 | 150 ms `ignore_until` debounce | Timing-based: fragile with async placement and slow apps. | Not needed with edge detection (our restore never produces a zoomed edge). The hotkey's own native maximize uses a one-shot "allow native" token (with a 2 s expiry). |
| D9 | `notify` crate directory watcher | A background thread plus cross-thread marshalling, for files that change rarely and are only needed at maximize time. | **Lazy reload:** at maximize time, compare file mtimes and re-parse if they changed. Monitor topology is invalidated by `WM_DISPLAYCHANGE` / `WM_SETTINGCHANGE(SPI_SETWORKAREA)`. No threads, no watcher. |
| D10 | Maximizing a window that already fills its zone is undefined | It would snap into the same zone again, so the maximize button appears to do nothing. | Configurable `already_in_zone`: `"restore"` (default; a second maximize toggles back to the pre-snap size, matching double-click habits) or `"maximize"` (native full monitor). Windows snapped by FancyZones itself (no known pre-snap rect) always go native. |
| D11 | Drag-to-top: "track rect during the drag" | During an Aero-snap drag, the pre-maximize rect isn't where the cursor is. | Track `EVENT_SYSTEM_MOVESIZESTART/END`. A maximize during (or within 500 ms after) a move/size loop picks the zone **nearest the cursor**. "Nearest", not "contains", because the cursor sits in the spacing gap at the top edge. |
| D12 | Shift bypass for "maximize" generally | `Win`+`Shift`+`Up` is Windows' own "stretch vertically" shortcut (no maximize happens), so Shift can't bypass the keyboard path. | Shift (configurable: shift/ctrl/alt/none) bypasses the mouse paths. The keyboard path uses the hotkey. `Ctrl`+`Alt`+`Up` is a known Intel-graphics rotate hotkey, so it is configurable and a registration failure is logged. |
| D13 | Eligibility = visible, root, maximize box, not tool window, not cloaked | Misses FancyZones' own rules and exclusions. | Also: require `WS_THICKFRAME`, apply FancyZones' popup rule, skip owned windows unless FancyZones allows child-window snapping, skip shell/system classes, **honor FancyZones' "excluded apps"** list, and add our own `excluded_apps`. |
| D14 | Laptop/16:9 screens "have no multi-zone layout" | On the dev machine **every** monitor, including the laptop panel `BOE0A52`, has a 3-zone priority-grid applied, so that acceptance criterion fails without extra configuration. | Either set the monitor to "No layout" in the FancyZones editor, or use our config: `min_aspect_ratio` (e.g. `2.0`, ultrawide only) or `excluded_monitors` (match by PnP id such as `"BOE0A52"`). |
| D15 | Not covered | Gaps: double launch, Explorer restart, autostart, diagnosis. | Single instance (named mutex), re-add the tray icon on `TaskbarCreated`, "Start with Windows" (HKCU `Run`), a TOML config file, a log file, and a `--dump` command that prints the monitors, matched layouts and zone rectangles (`--json` for scripting). |
| D16 | Windows 11 | The dev machine runs Windows 10 22H2 (19045). | Nothing in the design is Win11-specific. The border margins differ by OS and are measured, not assumed. |
| D17 | Flat module list | Logic mixed with Win32 calls is hard to unit test. | A **pure core** (geometry, layout math, monitor matching, the maximize decision state machine, EDID parsing, config) with no Win32 calls, fully unit tested. A thin **Win32 shell** around it. |
| D18 | No build tooling | — | Taskfile.dev (`Taskfile.yml`) drives fmt, lint, test, build, run, dump, install, autostart and packaging. |

### Maximize flash: pre-emptive interception (added after v1 testing)

Out-of-context WinEvents are delivered **after** Windows has maximized the window, so on
that path the maximize animation plays briefly before the window restores into the zone.
The common gestures are therefore caught **before** Windows acts (`src/preempt.rs`):

* **Dedicated input thread** running `WH_MOUSE_LL` + `WH_KEYBOARD_LL` (low-level hooks
  run in our own process, so there is no injection). Windows holds every input event
  until a low-level hook returns and silently removes slow hooks, so this thread does
  nothing else. Its only blocking call is a `WM_NCHITTEST` on button presses (40 ms,
  `SMTO_ABORTIFHUNG | SMTO_BLOCK`). It posts the target window to the main thread, which
  places it, or performs the maximize itself (`WM_SYSCOMMAND/SC_MAXIMIZE` plus the
  allow-native token) when the monitor has no zones, the app is excluded, and so on.
* **Maximize button:** the press on `HTMAXBUTTON` and its release are swallowed. The main
  thread acts only if the release is still on the button (slide-off cancels), then
  re-activates the window with FancyZones' zero-input `SetForegroundWindow` trick.
* **Title-bar double-click:** low-level hooks never see `WM_LBUTTONDBLCLK`, so two
  `HTCAPTION` presses within `GetDoubleClickTime` and `SM_C[XY]DOUBLECLK` are recognized
  here. The second press and its release are swallowed. The first click's move loop is
  why intercepted gestures bypass the drag-to-top heuristic (`consider_drag = false`).
* **Win+Up:** Up is swallowed (including auto-repeat) while Win is held and no
  Shift/Ctrl/Alt is down. Each intercepted press immediately taps an unassigned mask key
  (VK 0xE8). Anything watching for a lone, held Win key then sees "Win + key", as it
  would natively. Without this, PowerToys Shortcut Guide (shown when Win is held alone
  for about 1 s) took focus after a few presses while Win was held. The Win *release* is then swallowed too and replayed behind an
  unassigned mask key (VK 0xE8), so the shell never sees a lone Win press, which would open
  Start. Replaying in order works even when all four key events arrive in one burst.
* **Still corrected after the fact:** drag to the top edge, title bars that don't answer
  `WM_NCHITTEST` with `HTCAPTION`/`HTMAXBUTTON`, programmatic and menu maximizes.
* Both are configurable (`preempt_mouse_maximize`, `preempt_win_up`). The hook thread
  only exists while at least one is on.

`EVENT_OBJECT_LOCATIONCHANGE` is also raised for the cursor and caret. Those events are
rejected on the first comparison (`idObject != OBJID_WINDOW`), so the cost is one
message dispatch per event. In practice this is unmeasurable (FancyZones uses the same hook).

## 3. Tech stack

* Rust 1.99 (pinned in `rust-toolchain.toml`), MSVC toolchain; `x86_64-pc-windows-msvc`,
  with optional `aarch64-pc-windows-msvc` (`task build:arm64`).
* Crates: `windows` 0.62, `serde` + `serde_json` (FancyZones files), `toml` (our config),
  `tracing` + `tracing-subscriber` (logging), `anyhow`. **Dropped:** `notify` (D9).
* `#![windows_subsystem = "windows"]` in release builds only. Debug builds keep a console.

## 4. Module structure

```
src/
├── main.rs            CLI, single instance, DPI awareness, COM init, run loop
├── app.rs             hidden message window, wndproc, hotkey, reload, wiring
├── hooks.rs           SetWinEventHook registration + callback → engine
├── engine.rs          PURE maximize state machine (per-HWND state, decide())
├── geometry.rs        PURE Rect math, overlap, nearest-zone, inflate/deflate
├── config.rs          PURE-ish TOML config (defaults, parse, load/save)
├── fancyzones/
│   ├── mod.rs         data dir, mtime-cached loading of FancyZones files
│   ├── model.rs       PURE serde models (applied/custom layouts, settings, VD)
│   ├── layout.rs      PURE port of LayoutConfigurator (zone geometry)
│   └── matching.rs    PURE MonitorId/DeviceId equality + layout lookup
├── monitors.rs        EnumDisplayMonitors/Devices, EDID serial (PURE parser)
├── vdesktop.rs        IVirtualDesktopManager + registry fallbacks
├── window.rs          eligibility, placement, frame margins, workspace coords
├── tray.rs            Shell_NotifyIconW, menu, runtime-drawn icon
├── autostart.rs       HKCU\...\Run
└── logging.rs         file/console tracing setup
```

## 5. Behaviour specification

### 5.1 Event handling (all on the UI thread, `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`)

| Event | Action |
|-------|--------|
| startup | `EnumWindows`: seed `zoomed` state and border margins for candidate windows |
| `EVENT_OBJECT_SHOW` | seed state (a window *shown* maximized is not snapped) |
| `EVENT_OBJECT_DESTROY` | drop state |
| `EVENT_SYSTEM_MOVESIZESTART` | mark `in_move_size` |
| `EVENT_SYSTEM_MOVESIZEEND` | clear the mark, stamp the time, re-measure margins |
| `EVENT_OBJECT_LOCATIONCHANGE` | cheap reject unless `OBJID_WINDOW/CHILDID_SELF`; skip if minimized; edge-detect the zoomed state; on a rising edge → §5.2; while a verify is pending and the window is normal → §5.3 |

### 5.2 Maximize decision (`engine::decide`)

1. Paused → native.
2. One-shot "allow native" token present (hotkey) → consume it → native.
3. Bypass modifier held → native.
4. Window not eligible (D13) → native.
5. Resolve the monitor (`MonitorFromWindow`), the virtual desktop and the FancyZones
   layout → zones in screen coordinates. Fewer than 2 zones, or the monitor is filtered
   out (D14) → native.
6. Reference: inside or near a move/size loop → zone nearest the cursor. Otherwise → the
   zone with the largest overlap with `rcNormalPosition` (ties: contains the center, then
   nearest center, then lowest index).
7. If the window already fills that zone (within 3 px): `already_in_zone = "restore"` and
   a known pre-snap rect → restore to it. Otherwise → native.
8. Otherwise snap: target = zone grown by the cached margins (or estimated ones),
   `SetWindowPlacement(SW_RESTORE, rcNormalPosition = to_workspace(target), WPF_ASYNCWINDOWPLACEMENT)`.
   Record the snap (zone, pre-snap rect) and arm the verify.

### 5.3 Verify

On the first non-zoomed location change within 1 s after a snap: measure the margins,
cache them, and if the visible frame is off the zone by more than 1 px, issue one
`SetWindowPos` correction.

### 5.4 Hotkey (default `Ctrl`+`Alt`+`Up`)

> **Removed in 0.3.** Win + the bypass key + Up (Win+Shift+Up by default) covers this:
> the keyboard path for a normal full-screen maximize, alongside the bypass key held
> with the mouse. Older config files with `hotkey = ...` still load; the key is ignored.

Foreground window: if not maximized → arm the "allow native" token and
`ShowWindowAsync(SW_MAXIMIZE)`. If maximized → `ShowWindowAsync(SW_RESTORE)` (toggle).

### 5.5 Tray

Pause/Resume (also on double-click), Reload, Open config, Open log folder, Start with
Windows (checkbox), Exit. The icon (3 zones, 25/50/25) is drawn at runtime at the system
small-icon size and greys out while paused. It is re-added on `TaskbarCreated`.

## 6. Configuration (`%APPDATA%\fz-maximize\config.toml`, optional)

```toml
bypass_modifier = "shift"        # shift | ctrl | alt | none
already_in_zone = "restore"      # restore | maximize
snap_on_drag_maximize = true     # drag-to-top-edge goes to the zone under the cursor
hotkey = "Ctrl+Alt+Up"           # "" disables it
min_aspect_ratio = 0.0           # e.g. 2.0 = only ultrawide monitors are intercepted
excluded_monitors = []           # PnP ids / instance ids / \\.\DISPLAYn, e.g. ["BOE0A52"]
excluded_apps = []               # exe names or title substrings (case-insensitive)
respect_fancyzones_excluded_apps = true
log_level = "info"               # error | warn | info | debug | trace
```

## 7. Edge cases

* **Elevated windows:** UIPI makes `SetWindowPlacement` fail with access denied. This is
  logged at debug level and the window stays natively maximized. Running fz-maximize
  elevated would cover those windows. That is not done by default.
* **Hung windows:** `WPF_ASYNCWINDOWPLACEMENT` and `ShowWindowAsync` never block the
  UI thread.
* **Re-entrancy:** a COM call or modal tray menu can re-enter the hook callback. COM is
  initialized MTA (no message pumping on calls), and engine state is never borrowed
  across an external call; a busy borrow drops the event with a warning.
* **Taskbar top/left:** `rcNormalPosition` is in workspace coordinates, converted with
  FancyZones' two-pass `ScreenToWorkAreaCoords` algorithm (and its inverse).
* **Minimize/restore of a full-maximized window:** the zoomed state is frozen while
  iconic, so restoring isn't treated as a new maximize.
* **Monitor unplug/dock:** the topology cache is invalidated on `WM_DISPLAYCHANGE`. Zones
  are always recomputed from the current `rcWork`.

## 8. Testing

* **Unit tests (no desktop needed):** every template and zone count against
  hand-computed rectangles (including the 25/50/25 priority-grid on 7680×2160 and on the
  dev laptop), custom grid with merged cells, canvas scaling, spacing rules, device-id
  parsing, FancyZones equality semantics (serial mismatch, instance mismatch → number),
  virtual-desktop fallback order, EDID serial parsing, overlap/nearest selection, the
  full decide() matrix (paused, bypass, token, drag, toggle, already-in-zone,
  pass-through), workspace↔screen conversion, config parsing and hotkey parsing.
* **Fixture test:** the real `applied-layouts.json` from the dev machine (in
  `tests/fixtures/`) is parsed and resolved.
* **Live smoke test (`task e2e`):** starts fz-maximize, launches Notepad, maximizes it
  through the Win32 API, and checks that its frame equals the zone reported by
  `fz-maximize --dump --json`.

## 9. Acceptance criteria (revised)

1. Ultrawide 25/50/25: maximize (button, double-click, `Win`+`Up`, drag to top) on a
   window in the left, center or right zone places it flush in that zone.
2. Shift + maximize button or double-click → full-monitor maximize.
3. Hotkey → full-monitor maximize of the foreground window. Pressing it again restores.
4. Monitors with no or single-zone layouts, or filtered by `min_aspect_ratio` /
   `excluded_monitors`, behave natively. Docking/undocking needs no restart.
5. Editing layouts in the FancyZones editor takes effect on the next maximize, with no
   reload needed.
6. Idle CPU 0% with no input, working set < 15 MB, no input lag (no input hooks are
   installed at all).

## 9b. Settings window and the bypass modifier (added after v1 testing)

* **Bypass modifier** (`bypass_modifier`: Shift, Ctrl, Alt or none) applies to all three
  gestures: maximize button, title-bar double-click and Win + modifier + Up
  (`win_up_bypass`, on by default; it replaces Windows' Win+Shift+Up "stretch vertically").
  For mouse gestures, the input hook sends a "bypass" notice to the main thread *at
  press time* (an allow-native token), so the native maximize isn't redirected even if
  the modifier is released before Windows maximizes.
* **Settings window** (`src/settings.rs`): plain Win32 controls, so no UI framework is
  needed and the binary stays under 1 MB. An embedded manifest (`assets/fz-maximize.manifest`,
  linked by `build.rs` with `/MANIFEST:EMBED`, no resource compiler) provides themed common
  controls and per-monitor-v2 DPI awareness. Two columns, about 500 DIPs tall so it fits a
  1080p laptop at 150%. Fonts and layout are rebuilt on `WM_DPICHANGED`. OK/Apply validates,
  writes `config.toml` with `config::render` (the documented template with the values
  filled in) and applies it live. Opened from the tray or with `fz-maximize --settings`,
  which posts to the running instance and grants it foreground rights.

## 10. Staying in sync with FancyZones

**What is shared and what is re-implemented.** All layout *data* (which layout applies
to which monitor and desktop, template parameters, custom layouts, excluded apps) is read
from FancyZones' own JSON files. Edits made in the FancyZones editor take effect at the
next maximize. Only two *computations* are re-implemented, because FancyZones exposes
neither: layout → zone rectangles (`LayoutConfigurator`), and monitor identification and
matching. There is no supported way to ask FancyZones to place a window. `FancyZonesCLI`
(shipped with PowerToys) only lists and sets layouts and hotkeys and returns no
rectangles. FancyZones' private window messages are internal and less stable than the
file format.

**Failure mode.** Most upstream changes (file format, new layout types, monitor id
scheme) make data unparseable or unmatched, which means *no layout*, which means a
normal maximize. Only a change to the geometry math gives misplaced windows.

**Safeguard 1: runtime agreement check (`src/drift.rs`).** On every move/size loop,
fz-maximize records the window's FancyZones stamp (`FancyZones_zones`, the zone-index
bitmask FancyZones sets in `StampZoneIndexProperty`) and the drop position. Once the drop
settles (350 ms, one-shot timer, retried up to 2 s), a stamp that is new or a window that
FancyZones moved counts as a FancyZones snap. The window's visible frame is compared with
our rectangle for the stamped zones (bounding box for spans, ±2 px). The outcome is one
of: match; mismatch; *inconclusive* (same origin, different size, i.e. the app constrained
its own size); no layout resolved; or zone index out of range. Two distinct mismatching
samples (different window or zones) on one monitor+layout confirm drift. That raises one
tray notification and, with `on_fancyzones_mismatch = "native"`, turns redirection off
there. Counters are per PowerToys version (read from PowerToys' `settings.json`) and are
persisted in `%LOCALAPPDATA%\fz-maximize\fancyzones-check.json`, so `--dump` can say
whether the installed version has been verified. `--dump` also audits every currently
stamped window. Cost: one `GetPropW` + one DWM query per drag; nothing while idle.

**Safeguard 2: `task upstream:check` (`scripts/upstream-check.ps1`).** It compares the git
blob SHAs of the 23 upstream files the port mirrors (pinned in
`upstream/fancyzones-baseline.json`, verified byte-identical to the sources the port was
written from) with PowerToys `main` or a tag. Changed files are listed with their impact,
the fz-maximize module to review, and the upstream commits. Exit code 1 makes it
CI-friendly; it needs network access, so it is kept out of the offline `task ci`.
`task upstream:accept` re-pins after review.

## 11. Verification status (dev machine: Windows 10 22H2, PowerToys v0.102, 3840×2400 @ 250%)

| Check | Result |
|-------|--------|
| `task check` (fmt, clippy `-D warnings`, 124 unit/fixture tests) | pass |
| `task e2e`: maximize → zone 1 and zone 2 | visible frame equals the zone exactly (0 px error), with no border correction needed |
| `task e2e`: second maximize toggles back | restored to the exact pre-snap rect |
| `task e2e`: Shift + maximize | native full maximize (frame = work area) |
| `task e2e`: Ctrl+Alt+Up twice | native maximize, then restore |
| `task e2e`: Win+Up | lands in the zone, 0 px error |
| `task e2e`: real clicks on the maximize button, title-bar double-click (both directions), Win+Up | zone reached, `IsZoomed` polled every 1 ms never true (no flash), window stays focused, no Start menu; Shift + max button still native |
| `task e2e`: real FancyZones Shift-drag | FancyZones placed the window exactly on fz-maximize's zone 1; the runtime check logged `match` (3/3 suite runs) |
| `task upstream:check` | exit 0 vs main; exit 1 with 10 files flagged vs `v0.80.0` (negative test) |
| Release build idle footprint | 9.6 MB working set (1.7 MB private), 0 ms CPU over 10 s, 822 KB exe |

Not covered by automation (exercised through the same code path, but not synthesized):
real mouse clicks on the maximize button and title-bar double-clicks, drag-to-top-edge,
multi-monitor/ultrawide hardware, and virtual-desktop switching. The detection is
state-based (zoomed edge), so it doesn't depend on how the maximize was triggered.
The drag path and the multi-monitor/desktop resolution are unit tested.

Observed on the dev machine: the Intel driver reports **no monitor device** under
`\\.\DISPLAY1` (`EnumDisplayDevicesW(.., EDD_GET_DEVICE_INTERFACE_NAME)` returns nothing),
so FancyZones falls back to the display name as the monitor id. That is why
`applied-layouts.json` contains a `"monitor": "\\\\.\\DISPLAY1"` entry with an empty
instance. fz-maximize applies the same fallback and therefore resolves the same layout
FancyZones is using.
