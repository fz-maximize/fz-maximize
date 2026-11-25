# Original proposal (as provided)

> Kept for reference. See [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) for the
> revised plan that the code implements and for the list of deviations.

## Implementation Plan: fz-maximize (Maximize-to-FancyZone Utility)

### 1. Objective & Scope
Build a lightweight, out-of-process Windows 11 background utility in Rust that intercepts window maximize events and snaps windows into the Microsoft PowerToys FancyZones zone they mostly occupy, instead of maximizing across the full monitor.

- **Default Maximize:** Clicking Maximize, double-clicking the title bar, or pressing Win + Up snaps the window to the FancyZones zone with the largest overlap.
- **Full-Desktop Override:** Holding Shift while maximizing (or pressing a configurable global hotkey like Ctrl + Alt + Up) bypasses snapping and maximizes natively across the entire physical monitor.
- **Automatic Profiles:** Inherits per-monitor layouts directly from FancyZones. Monitors without a multi-zone layout (e.g., laptop screen or standard 16:9 display) behave normally with zero intervention.
- **Strict Constraints:** No DLL injection, no explorer.exe hooking, no admin privileges required for normal windows, and no polling loops.

### 2. Tech Stack & Dependencies
- Language: Rust (MSVC toolchain, x86_64-pc-windows-msvc / aarch64-pc-windows-msvc).
- Crates:
  - `windows = "0.62"` with features: Win32_Foundation, Win32_Graphics_Dwm, Win32_Graphics_Gdi, Win32_System_LibraryLoader, Win32_UI_Accessibility, Win32_UI_HiDpi, Win32_UI_Input_KeyboardAndMouse, Win32_UI_Shell, Win32_UI_WindowsAndMessaging
  - `serde` + `serde_json` — parse FancyZones JSON configuration files.
  - `notify` — watch the FancyZones config directory for layout edits.
  - `tracing` + `tracing-subscriber` — structured logging (file/console for debugging).
- Subsystem: Configure `#![windows_subsystem = "windows"]` for release builds so no console window appears.

### 3. Proposed Module Structure
```
src/
├── main.rs          # DPI awareness, hidden message window, Win32 message loop
├── fancyzones.rs    # Parse applied-layouts.json, custom-layouts.json, layout-templates.json
├── monitors.rs      # EnumDisplayMonitors, work-area (rcWork) resolution, zone RECT math
├── window.rs        # Window eligibility filters, DWM border compensation, SetWindowPos
├── hook.rs          # SetWinEventHook callbacks, last-normal-rect cache, maximize state machine
└── tray.rs          # Minimal Win32 Shell_NotifyIconW tray (Pause/Resume, Reload, Exit)
```

### 4. Phased Implementation

#### Phase 1: FancyZones Config Parser & Monitor Mapping (fancyzones.rs, monitors.rs)
- Locate Config Directory: Resolve `%LOCALAPPDATA%\Microsoft\PowerToys\FancyZones\`.
- Parse Layout Files:
  - `applied-layouts.json`: Maps each monitor (device.monitor, device.monitor-instance, device.monitor-number, device.virtual-desktop) to an applied-layout (uuid, type, show-spacing, spacing, zone-count).
  - `custom-layouts.json`: Stores user-defined Grid (rows, columns, rows-percentage, columns-percentage, cell-child-map) and Canvas (ref-width, ref-height, zones with X, Y, width, height) layouts.
  - `layout-templates.json`: Fallback for built-in templates (columns, rows, grid, priority-grid, focus) when applied-layout.type is not "custom".
- Compute Exact Zone RECTs:
  - Enumerate active monitors via EnumDisplayMonitors and GetMonitorInfoW (MONITORINFOEXW) to obtain each monitor's work area (rcWork, which excludes the taskbar) and device name.
  - Match each active HMONITOR to its entry in applied-layouts.json.
  - Convert the active layout into a `Vec<RECT>` in screen coordinates:
    - Grid: Compute row/column boundaries from rows-percentage and columns-percentage (scaled out of 10,000) relative to rcWork, merge cells sharing the same ID in cell-child-map, and inset by spacing if show-spacing is true.
    - Canvas: Scale each zone's (X, Y, width, height) from (ref-width, ref-height) to rcWork.
  - Single-Zone Filter: If a monitor has no applied layout or zones.len() <= 1, mark that monitor as pass-through (do not intercept maximize).
- Live Reload:
  - Use notify on the FancyZones directory to reload and recompute zone RECTs when applied-layouts.json or custom-layouts.json changes.
  - Listen for WM_DISPLAYCHANGE on the hidden message window to re-enumerate monitors when docking/undocking.

#### Phase 2: Window Tracking & Event Hooks (hook.rs, window.rs)
- Register Out-of-Context Hooks: Call SetWinEventHook with `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS` for:
  - EVENT_OBJECT_LOCATIONCHANGE (detects maximize transitions and tracks normal window bounds).
  - EVENT_SYSTEM_MOVESIZEEND (records clean pre-maximize bounds after user drags/resizes).
  - EVENT_OBJECT_DESTROY (evicts closed HWNDs from the state map to prevent memory growth).
- Filter Eligible Windows (window.rs): Ignore any HWND that fails these checks:
  - `id_object == OBJID_WINDOW` and `id_child == CHILDID_SELF`.
  - `IsWindowVisible(hwnd)` is true and `IsIconic(hwnd)` is false.
  - Not a child window (`GetAncestor(hwnd, GA_ROOT) == hwnd`).
  - Not a tool window (`(WS_EX_TOOLWINDOW & ex_style) == 0`).
  - Has a maximize box (`(WS_MAXIMIZEBOX & style) != 0`).
  - Not cloaked (`DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED)` equals 0).
- Maintain Window State: Store a `HashMap<isize, WindowState>` keyed by HWND:
  - `last_normal_rect: RECT` — updated whenever the window is visible and `!IsZoomed(hwnd)` (and not currently snapped to a zone).
  - `snapped_zone: Option<usize>` — tracks whether we snapped this window into a zone.
  - `ignore_until: Instant` — short debounce/re-entrancy guard (~150 ms) so our own ShowWindow / SetWindowPos calls don't re-trigger the handler.

#### Phase 3: Maximize Interception & Snapping (hook.rs, window.rs)
When EVENT_OBJECT_LOCATIONCHANGE fires for an eligible HWND:
- Check Transition: If `!IsZoomed(hwnd)`, update last_normal_rect (if not currently snapped) and return.
- Check Bypass Conditions:
  - Utility is paused via tray menu → return.
  - Shift key is held (`(GetAsyncKeyState(VK_SHIFT) as u16 & 0x8000) != 0`) → return (allow native full-monitor maximize).
  - Active monitor for last_normal_rect (via MonitorFromRect) is pass-through (zones.len() <= 1) → return.
- Select Target Zone:
  - Compute the intersection area between last_normal_rect and every zone RECT on that monitor.
  - Pick the zone with the largest intersection area (fallback: zone containing the center point of last_normal_rect).
- Snap Without Visual Gaps:
  - Set the re-entrancy guard (ignore_until) for this HWND.
  - Call `ShowWindow(hwnd, SW_RESTORE)` so the window exits the native maximized state.
  - Compensate for Windows 11 Invisible Borders: Query GetWindowRect vs. `DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS)` to compute the left/right/bottom invisible shadow margins, and expand the target zone RECT by those margins so the visible window edge sits flush against the zone boundary.
  - Call `SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE)`.
  - Record `snapped_zone = Some(zone_index)`.

#### Phase 4: Hotkey Override & Tray Control (main.rs, tray.rs)
- DPI Awareness: Call `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` as the very first line of main() so all RECTs are in physical pixels.
- Global Full-Maximize Hotkey: Register Ctrl + Alt + Up via RegisterHotKey on the hidden message window. On WM_HOTKEY, set the re-entrancy guard for GetForegroundWindow() and call `ShowWindow(hwnd, SW_MAXIMIZE)` to force a full-desktop maximize.
- System Tray Icon: Minimal Win32 Shell_NotifyIconW tray menu with:
  - Pause / Resume (toggles interception on/off).
  - Reload FancyZones Layouts (manual refresh).
  - Exit (unhooks events, removes tray icon, exits cleanly).

### 5. Edge Cases to Handle Explicitly
- Re-entrancy Loop: ShowWindow(SW_RESTORE) and SetWindowPos immediately fire EVENT_OBJECT_LOCATIONCHANGE. The per-HWND ignore_until guard prevents infinite loops.
- Elevated Windows: SetWindowPos will return ERROR_ACCESS_DENIED on elevated apps (e.g., Task Manager) when running unelevated. Log at debug level and ignore gracefully—never panic.
- Multi-Monitor Drag-to-Top-Edge: If a user drags a window to the top edge of the ultrawide to trigger Aero Snap maximize, last_normal_rect should reflect the window's position during the drag so it snaps into the zone under the cursor rather than where the window started.
- Unit Tests: Write pure unit tests in fancyzones.rs and monitors.rs using sample applied-layouts.json and custom-layouts.json fixtures (including a 25% / 50% / 25% Grid layout) to verify zone RECT calculation and overlap-selection math without needing a live desktop.

### 6. Acceptance Criteria
- Ultrawide 25/50/25 Layout: Clicking Maximize or double-clicking the title bar on a window in the left, center, or right zone snaps it flush into that zone.
- Shift Bypass: Holding Shift while clicking Maximize or double-clicking the title bar maximizes the window across the entire dual-4K desktop.
- Hotkey Bypass: Pressing Ctrl + Alt + Up maximizes the active window across the entire desktop.
- Profile Switching: Disconnecting the ultrawide (or moving a window to a 16:9 laptop display without a multi-zone layout) restores normal Windows maximize behavior automatically.
- Resource Footprint: Idle CPU usage is 0%, memory usage stays under ~15 MB, and there is zero perceptible mouse or typing lag.
