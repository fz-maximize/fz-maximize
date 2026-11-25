//! User configuration (`%APPDATA%\fz-maximize\config.toml`). Every key is optional.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a maximize does when the window already fills its zone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlreadyInZone {
    /// Toggle back to the size/position the window had before we snapped it
    /// (falls back to a native maximize when that is unknown).
    #[default]
    Restore,
    /// Let Windows maximize across the whole monitor.
    Maximize,
}

/// What to do once FancyZones and fz-maximize are confirmed to disagree on a monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MismatchAction {
    /// Log + tray notification; keep redirecting maximize.
    #[default]
    Warn,
    /// Also stop redirecting maximize on that monitor/layout until PowerToys is updated
    /// (or the layout changes).
    Native,
}

/// Modifier that, held during a mouse maximize, keeps the native full-monitor maximize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BypassModifier {
    #[default]
    Shift,
    Ctrl,
    Alt,
    None,
}

/// How the maximize animation looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnimationStyle {
    /// The style of the running Windows (Zoom on Windows 10, Morph on Windows 11).
    #[default]
    Auto,
    /// Windows 10: the window zooms slightly into place at its new position, fading in.
    Zoom,
    /// Windows 11: the window's rectangle moves to the new one, crossfading.
    Morph,
}

impl AnimationStyle {
    pub fn resolve(self) -> crate::animate::Style {
        match self {
            AnimationStyle::Auto => crate::animate::Style::of_this_windows(),
            AnimationStyle::Zoom => crate::animate::Style::Zoom,
            AnimationStyle::Morph => crate::animate::Style::Morph,
        }
    }
}

/// Length of Windows' own maximize animation, used when `animation_match_windows` is on.
/// Windows has no setting or API for it (only on/off switches), so this is measured:
/// 217-219 ms on Windows 10 22H2 (`measure_windows_maximize_animation` in tests/e2e.rs).
pub const WINDOWS_ANIMATION_MS: u32 = 215;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bypass_modifier: BypassModifier,
    pub already_in_zone: AlreadyInZone,
    /// Drag-to-top-edge (Aero Snap) maximize goes to the zone nearest the cursor.
    pub snap_on_drag_maximize: bool,
    /// Lock zone-maximized windows like truly maximized ones: their edges don't resize,
    /// and dragging the title bar restores the pre-snap size (drag to restore).
    pub lock_zone_maximized: bool,
    /// Intercept maximize-button clicks and title-bar double-clicks before Windows
    /// maximizes (no maximize animation flash). Uses a low-level mouse hook.
    pub preempt_mouse_maximize: bool,
    /// Intercept Win+Up the same way (low-level keyboard hook).
    pub preempt_win_up: bool,
    /// Animate intercepted maximizes into the zone. Skipped automatically when Windows
    /// wouldn't animate a maximize itself (either of its animation switches is off).
    pub animate_maximize: bool,
    /// Zoom (Windows 10) or morph (Windows 11); `auto` matches the running Windows.
    pub animation_style: AnimationStyle,
    /// Use the length of Windows' own maximize animation ([`WINDOWS_ANIMATION_MS`]).
    pub animation_match_windows: bool,
    /// Animation length in milliseconds (50-1000) when not matching Windows.
    pub animation_ms: u32,
    /// Win + bypass modifier + Up (Win+Shift+Up by default) does a normal full-monitor
    /// maximize, like Shift + maximize button. Replaces Windows' own Win+Shift+Up
    /// ("stretch vertically"); set false to keep that.
    pub win_up_bypass: bool,
    /// Removed in 0.3 (the standalone full-screen hotkey; Win + bypass key + Up does that).
    /// Still accepted so older config files load; never written back.
    #[serde(rename = "hotkey", skip_serializing)]
    pub legacy_hotkey: Option<toml::Value>,
    /// Only intercept on monitors whose width/height ratio is at least this (e.g. 2.0
    /// for ultrawides). 0 disables the filter.
    pub min_aspect_ratio: f64,
    /// Monitors to leave alone, matched case-insensitively against the PnP id
    /// (`BOE0A52`), device instance, display name (`\\.\DISPLAY1`) or serial.
    pub excluded_monitors: Vec<String>,
    /// Apps to leave alone: exe names (`notepad.exe`) or window-title substrings,
    /// case-insensitive, same matching rules as FancyZones' "Excluded apps".
    pub excluded_apps: Vec<String>,
    pub respect_fancyzones_excluded_apps: bool,
    /// Monitors without an applied-layouts entry get FancyZones' default layout (this is
    /// what FancyZones itself does).
    pub use_fancyzones_default_layout: bool,
    /// Do nothing while the FancyZones module is disabled in PowerToys.
    pub require_fancyzones_enabled: bool,
    /// Compare our zones with FancyZones' own placements (Shift-drag snaps) to detect
    /// drift after PowerToys updates.
    pub fancyzones_check: bool,
    pub on_fancyzones_mismatch: MismatchAction,
    /// error | warn | info | debug | trace. The FZM_LOG environment variable overrides it.
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bypass_modifier: BypassModifier::Shift,
            already_in_zone: AlreadyInZone::Restore,
            snap_on_drag_maximize: true,
            lock_zone_maximized: true,
            preempt_mouse_maximize: true,
            preempt_win_up: true,
            animate_maximize: true,
            animation_style: AnimationStyle::Auto,
            animation_match_windows: true,
            animation_ms: 200,
            win_up_bypass: true,
            legacy_hotkey: None,
            min_aspect_ratio: 0.0,
            excluded_monitors: Vec::new(),
            excluded_apps: Vec::new(),
            respect_fancyzones_excluded_apps: true,
            use_fancyzones_default_layout: true,
            require_fancyzones_enabled: true,
            fancyzones_check: true,
            on_fancyzones_mismatch: MismatchAction::Warn,
            log_level: "info".into(),
        }
    }
}

/// The file written by "Open config" when none exists. Must parse to `Config::default()`.
pub const TEMPLATE: &str = r#"# fz-maximize configuration. All keys are optional; these are the defaults.
# Easiest: tray icon -> Settings... (which rewrites this file, keeping these comments).
# After editing by hand, use tray -> "Reload" (or restart).
# The sections match the Settings window. ("Start with Windows" isn't stored here.)

# ---- General ---------------------------------------------------------------------

# error | warn | info | debug | trace
log_level = "info"

# ---- Behavior --------------------------------------------------------------------

# Maximizing a window that already fills its zone:
#   "restore"  -> toggle back to its size before it was snapped
#   "maximize" -> normal full-monitor maximize
already_in_zone = "restore"

# Dragging a window to the top screen edge snaps it to the zone under the cursor.
snap_on_drag_maximize = true

# Lock zone-maximized windows like truly maximized ones: their edges don't resize, and
# dragging the title bar returns the window to its size from before the snap (drag to
# restore). Hold the bypass key to resize or move a locked window normally.
lock_zone_maximized = true

# Hold this while clicking Maximize / double-clicking the title bar to get the normal
# full-monitor maximize: "shift" | "ctrl" | "alt" | "none"
bypass_modifier = "shift"

# Win + the bypass modifier + Up (Win+Shift+Up by default) gives a normal full-monitor
# maximize, like Shift + maximize button. This replaces Windows' own Win+Shift+Up
# ("stretch window vertically"); set false to keep Windows' behavior. Needs
# preempt_win_up.
win_up_bypass = true

# ---- Appearance ------------------------------------------------------------------

# No maximize flash: catch the maximize button / title-bar double-click / Win+Up
# *before* Windows maximizes, so the window goes straight into its zone. Other
# maximizes (drag to top, some custom title bars) are corrected right afterwards.
preempt_mouse_maximize = true
preempt_win_up = true

# Animate the window into its zone (only for the gestures above). Skipped whenever
# Windows wouldn't animate a maximize itself: "Animation effects" (Settings >
# Accessibility > Visual effects) or "Animate windows when minimizing and maximizing"
# (System Properties > Performance Options) is off.
animate_maximize = true
# How it looks: "auto" (matches this Windows), "zoom" (Windows 10: zooms slightly into
# place, fading in) or "morph" (Windows 11: moves to the zone, crossfading).
animation_style = "auto"
# Take as long as Windows' own maximize animation; false = use animation_ms instead.
animation_match_windows = true
animation_ms = 200

# ---- Exclusions ------------------------------------------------------------------

# Only work on monitors at least this wide relative to their height
# (e.g. 2.0 = ultrawides only). 0 = every monitor with a multi-zone layout.
min_aspect_ratio = 0.0

# Monitors to leave alone: PnP id ("BOE0A52"), instance, "\\.\DISPLAY1" or serial.
# Run `fz-maximize --dump` to see these values for your monitors.
excluded_monitors = []

# Apps to leave alone: exe names ("notepad.exe") or window-title substrings.
excluded_apps = []

# Also honor FancyZones' own "Excluded apps" list.
respect_fancyzones_excluded_apps = true

# ---- FancyZones integration ------------------------------------------------------

# Do nothing while FancyZones is disabled in PowerToys.
require_fancyzones_enabled = true

# Monitors without a saved FancyZones layout use FancyZones' default layout.
use_fancyzones_default_layout = true

# Each time FancyZones itself snaps a window (Shift-drag), compare where it put the
# window with where fz-maximize computes that zone. Detects drift after PowerToys updates.
fancyzones_check = true

# Once a disagreement is confirmed: "warn" (log + notification) or "native"
# (also stop redirecting maximize on that monitor until PowerToys/the layout changes).
on_fancyzones_mismatch = "warn"
"#;

/// Render `cfg` as a config file: [`TEMPLATE`] (with all its documentation) with each
/// `key = value` line set to `cfg`'s value. Used by the settings window.
pub fn render(cfg: &Config) -> String {
    let values: toml::Table = toml::Table::try_from(cfg).unwrap_or_default();
    let mut out = String::with_capacity(TEMPLATE.len() + 64);
    for line in TEMPLATE.lines() {
        let key = line
            .split_once(" = ")
            .map(|(k, _)| k)
            .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        match key.and_then(|k| values.get(k).map(|v| (k, v))) {
            Some((k, v)) => out.push_str(&format!("{k} = {v}")),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Validate and write `cfg` to `path` (atomically: temp file + rename).
pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
    cfg.validate()?;
    let text = render(cfg);
    parse(&text).map_err(|e| format!("internal error rendering config: {e}"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// `%APPDATA%\fz-maximize`
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("fz-maximize"))
}

pub fn default_config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("config.toml"))
}

pub fn parse(text: &str) -> Result<Config, String> {
    let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

/// Load the config; a missing file yields defaults. Errors are returned to the caller,
/// which reports them and falls back to defaults.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write [`TEMPLATE`] if the file does not exist yet.
pub fn ensure_exists(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, TEMPLATE)?;
    }
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if !(50..=1000).contains(&self.animation_ms) {
            return Err("animation_ms must be between 50 and 1000".into());
        }
        if !(self.min_aspect_ratio.is_finite() && self.min_aspect_ratio >= 0.0) {
            return Err("min_aspect_ratio must be a non-negative number".into());
        }
        if !matches!(
            self.log_level.to_ascii_lowercase().as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            return Err(format!("unknown log_level '{}'", self.log_level));
        }
        Ok(())
    }
}

impl Config {
    /// How long an intercepted maximize animates.
    pub fn animation_duration(&self) -> std::time::Duration {
        let ms = if self.animation_match_windows {
            WINDOWS_ANIMATION_MS
        } else {
            self.animation_ms
        };
        std::time::Duration::from_millis(u64::from(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_length_follows_windows_unless_overridden() {
        let mut c = Config::default();
        assert!(c.animation_match_windows, "matching Windows is the default");
        assert_eq!(c.animation_duration().as_millis(), u128::from(WINDOWS_ANIMATION_MS));
        c.animation_match_windows = false;
        c.animation_ms = 400;
        assert_eq!(c.animation_duration().as_millis(), 400);
    }

    #[test]
    fn template_is_the_default_config() {
        assert_eq!(parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn render_of_defaults_is_the_template() {
        assert_eq!(render(&Config::default()), TEMPLATE);
    }

    #[test]
    fn render_roundtrips_every_field() {
        let cfg = Config {
            bypass_modifier: BypassModifier::Alt,
            already_in_zone: AlreadyInZone::Maximize,
            snap_on_drag_maximize: false,
            lock_zone_maximized: false,
            preempt_mouse_maximize: false,
            preempt_win_up: false,
            animate_maximize: false,
            animation_style: AnimationStyle::Morph,
            animation_match_windows: false,
            animation_ms: 333,
            win_up_bypass: false,
            legacy_hotkey: None,
            min_aspect_ratio: 2.25,
            excluded_monitors: vec!["BOE0A52".into(), r"\.DISPLAY2".into()],
            excluded_apps: vec!["notepad.exe".into(), "Quote \"me\"".into()],
            respect_fancyzones_excluded_apps: false,
            use_fancyzones_default_layout: false,
            require_fancyzones_enabled: false,
            fancyzones_check: false,
            on_fancyzones_mismatch: MismatchAction::Native,
            log_level: "debug".into(),
        };
        let text = render(&cfg);
        assert_eq!(parse(&text).unwrap(), cfg);
        assert!(
            text.contains("# Hold this while clicking Maximize"),
            "documentation kept"
        );
    }

    #[test]
    fn save_validates_and_writes() {
        let dir = std::env::temp_dir().join(format!("fzm-save-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut cfg = Config {
            bypass_modifier: BypassModifier::Ctrl,
            ..Config::default()
        };
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path).unwrap(), cfg);
        cfg.animation_ms = 5;
        assert!(save(&path, &cfg).is_err());
        assert_eq!(
            load(&path).unwrap().bypass_modifier,
            BypassModifier::Ctrl,
            "invalid config not written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn partial_override() {
        let c = parse(
            "already_in_zone = \"maximize\"\nbypass_modifier = \"alt\"\nmin_aspect_ratio = 2.0\nexcluded_monitors = [\"BOE0A52\"]\nhotkey = \"\"",
        )
        .unwrap();
        assert_eq!(c.already_in_zone, AlreadyInZone::Maximize);
        assert_eq!(c.bypass_modifier, BypassModifier::Alt);
        assert_eq!(c.min_aspect_ratio, 2.0);
        assert_eq!(c.excluded_monitors, vec!["BOE0A52"]);
        assert!(c.legacy_hotkey.is_some(), "old configs with a hotkey still load");
        assert!(
            !render(&c).contains("hotkey"),
            "and the removed setting is dropped on save"
        );
        assert!(c.snap_on_drag_maximize);
    }

    #[test]
    fn invalid_configs_are_rejected() {
        assert!(parse("bypass_modifer = \"shift\"").is_err(), "typos are reported");
        assert!(parse("already_in_zone = \"sometimes\"").is_err());
        assert!(parse("min_aspect_ratio = -1.0").is_err());
        assert!(parse("log_level = \"loud\"").is_err());
        assert!(parse("animation_ms = 5").is_err());
        assert!(parse("animation_ms = 5000").is_err());
        assert!(parse("on_fancyzones_mismatch = \"panic\"").is_err());
        assert_eq!(
            parse("on_fancyzones_mismatch = \"native\"")
                .unwrap()
                .on_fancyzones_mismatch,
            MismatchAction::Native
        );
    }

    #[test]
    fn load_missing_file_is_default_and_ensure_exists_writes_template() {
        let dir = std::env::temp_dir().join(format!("fzm-cfg-test-{}", std::process::id()));
        let path = dir.join("sub").join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(load(&path).unwrap(), Config::default());
        ensure_exists(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEMPLATE);
        assert_eq!(load(&path).unwrap(), Config::default());
        std::fs::write(&path, "nope = 1").unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
