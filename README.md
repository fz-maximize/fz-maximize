# fz-maximize

Make the Windows **maximize** action fill the [PowerToys FancyZones](https://learn.microsoft.com/windows/powertoys/fancyzones)
zone a window is in, instead of the whole monitor. Built for ultrawide monitors.

* Maximize button, title-bar double-click, `Win`+`Up` and drag-to-top-edge all snap the
  window into the zone it mostly overlaps (drag-to-top uses the zone under the cursor).
* Maximize again to toggle back to the window's previous size.
* Hold **Shift** while maximizing, or press **Ctrl+Alt+Up**, for a normal full-monitor maximize.
* Layouts come straight from FancyZones (templates, custom grid and canvas layouts, per
  monitor and per virtual desktop). Monitors with no layout or a single zone behave normally.
* Small and passive: no DLL injection, no input hooks, no admin rights, no polling.
  About 10 MB of RAM and 0% CPU when idle.

Design notes, including how this deviates from the original proposal, are in
[docs/IMPLEMENTATION_PLAN.md](docs/IMPLEMENTATION_PLAN.md).

## Requirements

* Windows 10 or 11 with PowerToys and FancyZones enabled.
* To run: nothing beyond Windows itself. The exe only uses DLLs that ship with Windows
  (its C runtime is linked in, the "hybrid CRT" PowerToys also uses).
* To build: Rust (the version pinned in `rust-toolchain.toml` is installed automatically
  by rustup), the MSVC build tools, [Task](https://taskfile.dev) (`winget install Task.Task`)
  and, for the installer, Inno Setup (`task setup:installer`).

## Quick start

```powershell
task setup            # toolchain + dependencies
task setup:installer  # Inno Setup (per-user), for building the installer
task dump             # see what fz-maximize detects: monitors, FancyZones layout, zones
task install          # build the installer and install per-user: starts with Windows, starts now
```

A tray icon (three zones on a blue tile) appears. Right-click it for:
**Settings...**, Pause/Resume (or double-click the icon), Reload layouts & config,
Open config file, Open log folder, Start with Windows, Exit.

`task uninstall` (or Settings → Apps → fz-maximize) removes the program, shortcut and
autostart entry. Config and logs are kept. See [docs/RELEASING.md](docs/RELEASING.md)
for how releases are built, signed and distributed.

## Behaviour

| You do | Result |
|---|---|
| Maximize a floating window | It fills the zone it overlaps most (the zone's spacing gaps are respected) |
| Maximize a window that we snapped | It goes back to its size before the snap (`already_in_zone = "restore"`) |
| Maximize a window FancyZones snapped | Native full-monitor maximize (it already fills its zone) |
| Drag a window to the top screen edge | It fills the zone nearest the cursor |
| Hold Shift + maximize button / double-click, or Win+Shift+Up | Native full-monitor maximize (the key is configurable: Shift, Ctrl, Alt or none) |
| `Ctrl`+`Alt`+`Up` | Native maximize of the active window; press again to restore |
| Window on a monitor with 0–1 zones | Native maximize |
| Elevated (admin) window | Native maximize (Windows blocks non-admin apps from moving it) |

FancyZones edits take effect at the next maximize; no reload is needed. Docking, undocking
and taskbar changes are picked up automatically.

## Staying in sync with FancyZones

fz-maximize reads all layout *data* from FancyZones' own files, so anything you change
in the FancyZones editor applies at the next maximize. It does re-implement two
*computations*, the zone geometry and the monitor matching (FancyZones has no API that
exposes either). Two safeguards cover upstream changes:

* **Agreement check (runtime, on by default).** Each time FancyZones itself snaps a window
  (Shift-drag), fz-maximize reads the zone FancyZones stamped on the window and compares
  where FancyZones put it with where fz-maximize computes that zone. Results show in the
  tray menu ("FancyZones agreement: 12/12 match") and in `task dump`. Two disagreeing
  snaps on different windows or zones trigger a notification. Counters restart when
  PowerToys is updated, so you can see whether the new version has been verified.
  Set `on_fancyzones_mismatch = "native"` to also fall back to normal maximize there.
* **`task upstream:check` (development/CI).** It diffs the PowerToys source files the
  port mirrors (pinned in `upstream/fancyzones-baseline.json`) against PowerToys `main`
  or a release (`task upstream:check -- -Ref v0.103.0`). It exits 1 and lists which
  fz-maximize module to review when something changed. After updating the port, run
  `task upstream:accept`.

If an upstream change does break something, the usual failure is a normal maximize,
not a misplaced window: data fz-maximize can't parse or match means "no layout".

**No maximize flash for the common gestures.** Maximize-button clicks, title-bar
double-clicks and `Win`+`Up` are caught *before* Windows maximizes, by low-level
mouse/keyboard hooks on a dedicated thread (no DLL injection), so the window moves
straight into its zone. Other maximizes (drag to the top edge, apps whose title bars
don't report themselves to Windows, programmatic maximizes) are still corrected right
after Windows maximizes, so the maximize animation briefly shows for those.
Interception can be turned off with `preempt_mouse_maximize` / `preempt_win_up`.

Those intercepted moves are **animated** into the zone (`animate_maximize`,
`animation_ms`), so they feel like Windows' own maximize. The animation is drawn the way Windows'
compositor draws its own (GPU-scaled live previews, at the display's refresh rate), in
the style of the running Windows: Windows 10's zoom-and-fade, or Windows 11's morph
(`animation_style`). Skipped when Windows' animation effects are off.

## Settings

Tray icon → **Settings...** (or `task settings`) opens a settings window with every
option. OK/Apply saves `%APPDATA%\fz-maximize\config.toml` and applies it immediately;
no restart is needed.

## Configuration file

The settings window writes this file; you can also edit it by hand. `task config` opens
it, with every option documented. Apply hand edits with tray → Reload.

```toml
bypass_modifier = "shift"          # shift | ctrl | alt | none
already_in_zone = "restore"        # restore | maximize
snap_on_drag_maximize = true
preempt_mouse_maximize = true      # catch max button / title-bar double-click before Windows (no flash)
preempt_win_up = true              # same for Win+Up
win_up_bypass = true               # Win + bypass key + Up = full maximize (replaces Win+Shift+Up stretch)
animate_maximize = true            # animate intercepted maximizes into the zone
animation_style = "auto"           # "zoom" (Windows 10 look), "morph" (Windows 11 look); auto = this Windows
animation_ms = 200                 # animation length (50-1000)
hotkey = "Ctrl+Alt+Up"             # "" disables
min_aspect_ratio = 0.0             # e.g. 2.0 = only ultrawide monitors
excluded_monitors = []             # e.g. ["BOE0A52"] (ids from `task dump`)
excluded_apps = []                 # exe names or window-title substrings
respect_fancyzones_excluded_apps = true
use_fancyzones_default_layout = true
require_fancyzones_enabled = true
fancyzones_check = true            # compare with FancyZones' own snaps
on_fancyzones_mismatch = "warn"    # warn | native
log_level = "info"                 # FZM_LOG env var overrides
```

### Leaving a laptop screen alone

FancyZones applies its default layout (3-zone priority grid) to every monitor, laptop
panels included. To keep normal maximize on a screen, do one of these:

* in the FancyZones editor, choose **No layout** for that monitor, or
* set `min_aspect_ratio = 2.0` (only ultrawides are intercepted), or
* list the monitor in `excluded_monitors` (its id is shown by `task dump`).

## Tasks

| Task | What it does |
|---|---|
| `task` | list tasks |
| `task setup` | install/verify the pinned toolchain, fetch dependencies |
| `task build` / `task build:release` / `task build:arm64` | debug / optimized / Windows-on-ARM build |
| `task run` | run the debug build in the foreground with debug logs in the terminal |
| `task try` | run the release build from `target/` in the background with debug logging (no install) |
| `task dump` (`task dump -- --json`) | monitors, matched layouts, zones, and whether maximize is redirected |
| `task test` | unit + fixture tests (no desktop interaction) |
| `task e2e` | live test: opens Notepad; real clicks on the maximize button and title bar (never maximized = no flash), toggle, Shift, hotkey, Win+Up (no Start menu) and a real FancyZones Shift-drag |
| `task check` / `task ci` | fmt + clippy + tests (+ release build) |
| `task upstream:check` / `upstream:accept` | diff mirrored PowerToys sources against upstream / re-pin the baseline |
| `task install` / `task uninstall` | install silently via the real installer / uninstall |
| `task install:wizard` / `task uninstall:wizard` | run the installer / uninstaller with its normal UI, as users see it |
| `task install:wizard:downloaded` | same, marked as downloaded, to also see the SmartScreen warning |
| `task start` / `stop` / `restart` | control the installed copy |
| `task logs` (`task logs -- -Wait`) | tail the log |
| `task settings` | open the settings window of the running instance |
| `task config` | open (and create) the config file |
| `task package` | build the installer (+ `.sha256`) and portable zip into `dist/` |
| `task setup:installer` | install Inno Setup (per-user, via winget) |

## Command line

```
fz-maximize [--console] [--config <path>]       run (tray icon)
fz-maximize --dump [--json] [--config <path>]   diagnostics
fz-maximize --quit                              stop the running instance
fz-maximize --settings                          open the settings window of the running instance
fz-maximize --init-config                       create the config file
fz-maximize --autostart on|off|status
```

The release binary is a GUI-subsystem app, so in PowerShell pipe its output to see it
(`fz-maximize --dump | Out-String`), or use `task dump`, which runs the console build.

## Troubleshooting

1. Run `task dump`. For each monitor it shows the identity FancyZones uses, which
   `applied-layouts.json` entry matched (and for which virtual desktop), the zone
   rectangles, and why maximize is or isn't redirected.
2. `task logs` shows one line per redirected maximize. Set `log_level = "debug"` (or
   run `task run`) to see why a maximize was left alone.
3. If an app misbehaves, add it to `excluded_apps` (or to FancyZones' own excluded apps).

## Project layout

```
src/
  main.rs            CLI, single instance, DPI awareness
  app.rs             hidden window, message loop, maximize handling, --dump
  engine.rs          maximize state machine (pure, unit tested)
  preempt.rs         input hooks + gesture recognition (no-flash interception)
  drift.rs           FancyZones agreement check (pure, unit tested)
  policy.rs          per-monitor decision: zones or native (pure)
  geometry.rs        rect math, zone selection (pure)
  filters.rs         app / monitor exclusion rules (pure)
  config.rs          config.toml + hotkey parsing (pure)
  fancyzones/        FancyZones data: parsing, layout geometry port, monitor matching
  monitors.rs        monitor enumeration + EDID serials
  vdesktop.rs        virtual desktop ids
  window.rs          window eligibility, placement, border compensation
  settings.rs        settings window (native Win32, two columns, DPI-aware)
  hooks.rs tray.rs autostart.rs logging.rs win.rs
scripts/upstream-check.ps1   upstream diff (task upstream:check)
scripts/build-installer.ps1  builds the Inno Setup installer (task package:installer)
installer/fz-maximize.iss   Inno Setup script (per-user install, autostart, upgrade, uninstall)
.cargo/config.toml          hybrid CRT: no Visual C++ Redistributable needed
upstream/fancyzones-baseline.json   pinned upstream file versions
tests/
  e2e.rs             live desktop tests (ignored by default; `task e2e`)
  fixtures/          FancyZones JSON samples
```

## License

[MIT](LICENSE). Parts of fz-maximize are ported from
[Microsoft PowerToys](https://github.com/microsoft/PowerToys) (MIT); see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

fz-maximize is an independent project, not affiliated with or endorsed by Microsoft.
"PowerToys" and "FancyZones" are Microsoft product names, used only to describe
compatibility.
