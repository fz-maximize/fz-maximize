# Developing fz-maximize

How to build, run and test fz-maximize, how the code is organized, and how it stays in step
with FancyZones. For the contribution workflow (branches, PRs, merging) see
[CONTRIBUTING.md](../CONTRIBUTING.md); for releases see [RELEASING.md](RELEASING.md).
Design history: [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md).

## Prerequisites

* **Rust**: install [rustup](https://rustup.rs); the toolchain pinned in
  `rust-toolchain.toml` is installed automatically.
* **Visual Studio Build Tools** with "Desktop development with C++" (MSVC linker and
  Windows SDK).
* **[Task](https://taskfile.dev)**: `winget install Task.Task`.
* For the installer: **Inno Setup 6**, `task setup:installer` (per-user, via winget).
* For ARM64 builds: the Visual Studio "MSVC ARM64 build tools", `task setup:arm64:tools`.
* For the website: **Node.js LTS** (`winget install OpenJS.NodeJS.LTS`, or nvm-windows).
* For the live tests: PowerToys with FancyZones enabled and a multi-zone layout on the
  primary monitor.

None of these are needed to *run* fz-maximize. The exe links its C runtime statically (the
"hybrid CRT" in `.cargo/config.toml`, as PowerToys does) and only uses DLLs that ship with
Windows.

## Quick start

```powershell
task setup     # toolchain + dependencies
task dump      # what fz-maximize detects: monitors, FancyZones layout, zones, decisions
task run       # debug build in the foreground, debug logs in the terminal
task check     # fmt + clippy (warnings are errors) + unit tests; must pass for a PR
```

`task try` runs the release build from `target/` in the background with debug logging,
without installing; `task install` builds the real installer and installs it per-user.

## Tasks

| Task | What it does |
|---|---|
| `task` | list tasks |
| `task setup` | install/verify the pinned toolchain, fetch dependencies |
| `task build` / `build:release` / `build:arm64` | debug / optimized / Windows-on-ARM build |
| `task run` | run the debug build in the foreground with debug logs in the terminal |
| `task try` | stop any running copy, then run the release build from `target/` in the background with debug logging |
| `task dump` (`task dump -- --json`) | monitors, matched layouts, zones, and whether maximize is redirected |
| `task test` | unit and fixture tests (no desktop interaction) |
| `task e2e` | live desktop tests: opens Notepad and drives real clicks and keystrokes (maximize button, title bar, toggle, Shift, Win+Up, Win+Shift+Up, the window lock, animation, a real FancyZones Shift-drag). Don't touch the PC while it runs |
| `task check` / `task ci` | fmt + clippy + tests / the same plus release packages for all architectures |
| `task lint:arm64` | type-check the ARM64 build (works without the ARM64 tools) |
| `task upstream:check` / `upstream:accept` | diff mirrored PowerToys sources against upstream / re-pin the baseline |
| `task install` / `uninstall` | install silently via the real installer / uninstall |
| `task install:wizard` / `uninstall:wizard` | run the installer / uninstaller with its normal UI |
| `task install:wizard:downloaded` | the same, marked as downloaded, to see the SmartScreen warning |
| `task start` / `stop` / `restart` | control the installed copy |
| `task logs` (`task logs -- -Wait`) | tail the log |
| `task settings` / `task config` | open the settings window of the running copy / the config file |
| `task package` | installers (+ `.sha256`) and portable zips into `dist/`, x64 and ARM64 (`package:x64` / `package:arm64` for one) |
| `task site` / `site:build` | preview the website with live reload / build it as CI does |

The release binary is a GUI-subsystem app, so in PowerShell pipe its output to see it
(`fz-maximize --dump | Out-String`), or use `task dump`, which runs the console build.

## How it works

* **Events, not polling.** Out-of-context WinEvent hooks (`hooks.rs`) report window
  maximizes, moves and FancyZones snaps. `engine.rs` decides what to do; it's a pure state
  machine with time passed in, so it's unit tested without a desktop.
* **Catching maximize before Windows does** (`preempt.rs`). Low-level mouse and keyboard
  hooks on a dedicated thread recognize maximize-button clicks, title-bar double-clicks
  and Win+Up, swallow them and place the window directly, so there's no full-screen
  flash. No DLL is injected into other processes. Everything else (drag to the top edge,
  custom title bars, programmatic maximizes) is corrected right after Windows maximizes.
  The same hooks implement the window lock (`lock_zone_maximized`): resize drags on a
  zone-maximized window are blocked and title-bar drags restore it.
* **Animation** (`animate.rs`). DWM thumbnails of the window, scaled on the GPU and paced
  with `DwmFlush` at the display's refresh rate, in Windows 10's zoom style or Windows 11's
  morph style. Follows Windows' animation settings.
* **FancyZones data** (`fancyzones/`). Parses FancyZones' JSON files, computes zone
  rectangles the way FancyZones does (`layout.rs`), and matches monitors and virtual
  desktops to layouts (`matching.rs`).
* **Monitor identity** (`monitors.rs`). Mirrors FancyZones' `MonitorUtils`, with the EDID
  serial read from the registry. Windows sometimes can't report a monitor's identity
  (seen on hybrid-GPU laptops), so the last known identity per display is remembered in
  `%LOCALAPPDATA%\fz-maximize\monitors.json`.

## Staying in sync with FancyZones

fz-maximize reads all layout *data* from FancyZones' own files, so anything changed in the
FancyZones editor applies at the next maximize. It does re-implement two *computations*,
the zone geometry and the monitor matching, because FancyZones has no API that exposes
either. Two safeguards cover upstream changes:

* **Agreement check (runtime, on by default).** Each time FancyZones itself snaps a window
  (Shift-drag), fz-maximize reads the zone FancyZones stamped on the window and compares
  where FancyZones put it with where fz-maximize computes that zone. Results show in the
  tray menu ("FancyZones agreement: 12/12 match") and in `task dump`. Two disagreeing snaps
  on different windows or zones trigger a notification. Counters restart when PowerToys is
  updated. `on_fancyzones_mismatch = "native"` also falls back to normal maximize there.
* **`task upstream:check` (development/CI).** Diffs the PowerToys source files the port
  mirrors (pinned in `upstream/fancyzones-baseline.json`) against PowerToys `main` or a
  release (`task upstream:check -- -Ref v0.103.0`). It exits 1 and lists which fz-maximize
  module to review when something changed. After updating the port, run
  `task upstream:accept`.

If an upstream change does break something, the usual failure is a normal maximize, not a
misplaced window: data fz-maximize can't parse or match means "no layout".

## Project layout

```
src/
  main.rs            CLI, single instance, DPI awareness
  app.rs             hidden window, message loop, maximize handling, --dump
  engine.rs          maximize state machine (pure, unit tested)
  preempt.rs         input hooks: gesture recognition, no-flash interception, window lock
  animate.rs         maximize animation (DWM thumbnails)
  drift.rs           FancyZones agreement check (pure, unit tested)
  policy.rs          per-monitor decision: zones or native (pure)
  geometry.rs        rect math, zone selection (pure)
  filters.rs         app / monitor exclusion rules (pure)
  config.rs          config.toml (pure)
  fancyzones/        FancyZones data: parsing, layout geometry port, monitor matching
  monitors.rs        monitor enumeration, EDID serials, remembered identities
  vdesktop.rs        virtual desktop ids
  window.rs          window eligibility, placement, border compensation
  settings.rs        settings window (native Win32, DPI-aware)
  hooks.rs tray.rs autostart.rs logging.rs win.rs
tests/
  e2e.rs             live desktop tests (ignored by default; `task e2e`)
  fixtures/          FancyZones JSON samples
installer/fz-maximize.iss          Inno Setup script (per-user install, autostart, upgrade, uninstall)
packaging/winget/                  winget manifests (copies of what's submitted to winget-pkgs)
scripts/                           installer build, upstream diff, ARM64 tools check
site/                              the website (Astro Starlight), see site/README.md
upstream/fancyzones-baseline.json  pinned upstream file versions
.cargo/config.toml                 hybrid CRT: no Visual C++ Redistributable needed
```
