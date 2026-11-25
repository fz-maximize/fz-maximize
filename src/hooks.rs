//! Out-of-context WinEvent hooks. Callbacks are delivered on the installing thread via
//! its message loop, so all state stays single-threaded.

use std::sync::OnceLock;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_SHOW, EVENT_SYSTEM_MOVESIZEEND,
    EVENT_SYSTEM_MOVESIZESTART, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

/// The events we care about, reported to the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinEvent {
    Show,
    Destroy,
    MoveSizeStart,
    MoveSizeEnd,
    LocationChange,
}

impl WinEvent {
    fn from_raw(event: u32) -> Option<Self> {
        Some(match event {
            EVENT_OBJECT_SHOW => Self::Show,
            EVENT_OBJECT_DESTROY => Self::Destroy,
            EVENT_SYSTEM_MOVESIZESTART => Self::MoveSizeStart,
            EVENT_SYSTEM_MOVESIZEEND => Self::MoveSizeEnd,
            EVENT_OBJECT_LOCATIONCHANGE => Self::LocationChange,
            _ => return None,
        })
    }
}

pub type Handler = fn(WinEvent, HWND);

static HANDLER: OnceLock<Handler> = OnceLock::new();

unsafe extern "system" fn proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Hot path: LOCATIONCHANGE also fires for the cursor and carets on every move.
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd.is_invalid() {
        return;
    }
    let Some(ev) = WinEvent::from_raw(event) else { return };
    if let Some(h) = HANDLER.get() {
        h(ev, hwnd);
    }
}

/// Installed hooks; unhooked on drop.
pub struct Hooks(Vec<HWINEVENTHOOK>);

impl Hooks {
    /// Install the hooks. The first handler passed wins for the process lifetime.
    pub fn install(handler: Handler) -> windows::core::Result<Self> {
        let _ = HANDLER.set(handler);
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let ranges = [
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND),
            (EVENT_OBJECT_DESTROY, EVENT_OBJECT_SHOW),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
        ];
        let mut hooks = Vec::with_capacity(ranges.len());
        for (min, max) in ranges {
            let h = unsafe { SetWinEventHook(min, max, None, Some(proc), 0, 0, flags) };
            if h.is_invalid() {
                let err = windows::core::Error::from_thread();
                drop(Hooks(hooks));
                return Err(err);
            }
            hooks.push(h);
        }
        Ok(Self(hooks))
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_mapping() {
        assert_eq!(WinEvent::from_raw(EVENT_OBJECT_SHOW), Some(WinEvent::Show));
        assert_eq!(
            WinEvent::from_raw(EVENT_OBJECT_LOCATIONCHANGE),
            Some(WinEvent::LocationChange)
        );
        assert_eq!(
            WinEvent::from_raw(EVENT_SYSTEM_MOVESIZEEND),
            Some(WinEvent::MoveSizeEnd)
        );
        assert_eq!(WinEvent::from_raw(0x8003), None); // EVENT_OBJECT_HIDE: not subscribed
    }
}
