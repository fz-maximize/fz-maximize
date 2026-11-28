# Changelog

What's new in each fz-maximize release, newest first.

<!--
Each release gets a "## <version>" heading and "- " bullets written for people who use
fz-maximize, not developers. The app shows its own version's entry in About, the update
window shows the new version's entry, and each GitHub release starts with it. A unit test
checks that the version in Cargo.toml has an entry, so add one in the version bump PR.
Security release PRs add theirs automatically.
-->

## 0.4.7

- You can choose when the daily update check happens: Settings → General → Check at (3:00 AM by default). If your PC is off or asleep then, it checks as soon as it's on again.
- New option: Install new versions automatically, instead of asking. The daily check then installs a new version as soon as it finds it, and fz-maximize restarts for a moment. A version you chose to skip isn't installed.

## 0.4.6

- Clicking the title bar of a window maximized into a zone no longer returns it to its old size when the mouse moves slightly during the click. Like a maximized window, it now waits until the mouse has moved as far as Windows needs to start a drag (4 pixels by default).

## 0.4.5

- While an update is waiting, the tray icon shows a small yellow dot, so you still see it if you missed the notification. It stays until you install or skip the update, or pick a time to install it. The update is also the first item in the tray menu, and the icon's tooltip names it.
- After updating with the installer from the website, the tray menu no longer offers the version you already have.

## 0.4.4

- New About window (tray menu → About fz-maximize): which version you have, what's new in it, and a button that checks for updates and installs one.
- When an update is offered, the update window shows what's new in it.
- The tray icon's tooltip shows the version.

## 0.4.3

- The maximize animation no longer turns everything around the window black when the NVIDIA App is running. Its invisible full-screen overlay window showed up in the animation as solid black.

## 0.4.2

- The maximize animation never shows a black picture of the window: if the window's picture comes back black, another way of taking it is tried, and if that's black too, the window moves without an animation.
- A window partly covered by another one now animates without the other window in its picture.

## 0.4.1

- No changes to the app. Released to test updating from 0.4.0.

## 0.4.0

- Optional update check: once a day fz-maximize can check for a new version and offer to install it now, at a time you choose, remind you later, or skip it. It's off unless you turn it on, and it asks the first time it starts.
- The settings window has two columns: General, FancyZones integration and Behavior on the left, Appearance and Exclusions on the right.

## 0.3.4

- The program and installer now show their product name and version in their file properties.
- The tray menu's comparison with FancyZones' own snaps starts over when fz-maximize is updated.

## 0.3.3

- Win+Up starts its animation sooner: the window's picture is taken as the Windows key goes down.

## 0.3.2

- The maximize animation starts much sooner: about 20 ms after the click instead of about 250 ms.
- Links now point to the fz-maximize website.

## 0.3.1

- The installer closes a running fz-maximize wherever it runs from, and starts the new version afterwards.
- Monitors are recognized even when Windows can't report their identity (seen on laptops with two graphics chips).

## 0.3.0

- Windows maximized into a zone behave like maximized windows: their edges can't be dragged to resize them, and dragging the title bar restores them.
- The settings window groups related settings under headings.
- The Ctrl+Alt+Up hotkey for a normal full-screen maximize was removed: hold the bypass key (Shift by default) with Win+Up, or while clicking, instead.

## 0.2.1

- Native builds for Windows on ARM (ARM64).

## 0.2.0

- Maximizing into a zone is animated in the style of your Windows version (zoom on Windows 10, morph on Windows 11), following Windows' animation settings.

## 0.1.0

- First release: maximize fills the FancyZones zone a window is in, instead of the whole monitor.
