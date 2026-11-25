//! Virtual desktop identification.
//!
//! Primary source: the documented `IVirtualDesktopManager::GetWindowDesktopId`, which
//! gives the desktop of the very window being maximized. Fallbacks follow FancyZones'
//! `VirtualDesktop::GetCurrentVirtualDesktopIdFromRegistry`.

use std::cell::RefCell;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_BINARY, RegGetValueW};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use windows::core::{GUID, HSTRING, PCWSTR};

use crate::fancyzones::model::{NULL_GUID, guid_from_le_bytes};

thread_local! {
    static MANAGER: RefCell<Option<IVirtualDesktopManager>> = const { RefCell::new(None) };
}

pub fn format_guid(g: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

/// The virtual desktop `hwnd` lives on. `None` if unavailable (e.g. Explorer not
/// running) or the window is not assigned to a desktop.
pub fn window_desktop(hwnd: HWND) -> Option<String> {
    MANAGER.with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        if slot.is_none() {
            match unsafe { CoCreateInstance::<_, IVirtualDesktopManager>(&VirtualDesktopManager, None, CLSCTX_ALL) } {
                Ok(m) => *slot = Some(m),
                Err(e) => {
                    tracing::debug!(error = %e, "IVirtualDesktopManager unavailable");
                    return None;
                }
            }
        }
        let manager = slot.as_ref()?;
        match unsafe { manager.GetWindowDesktopId(hwnd) } {
            Ok(g) => {
                let s = format_guid(&g);
                (s != NULL_GUID).then_some(s)
            }
            Err(e) => {
                tracing::debug!(error = %e, "GetWindowDesktopId failed");
                // The COM server may have gone away with Explorer; recreate next time.
                *slot = None;
                None
            }
        }
    })
}

/// Drop the cached COM object (e.g. after Explorer restarted).
pub fn reset() {
    MANAGER.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            *slot = None;
        }
    });
}

fn read_guid_value(subkey: &str, value: &str) -> Option<Vec<u8>> {
    let subkey = HSTRING::from(subkey);
    let value = HSTRING::from(value);
    let mut buf = vec![0u8; 16 * 64];
    let mut size = buf.len() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

const VD_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\VirtualDesktops";

/// The current virtual desktop according to the registry (newer builds' global key,
/// then the per-session key Windows 10 uses, then the first known desktop).
pub fn registry_current() -> Option<String> {
    if let Some(g) = read_guid_value(VD_KEY, "CurrentVirtualDesktop").and_then(|b| guid_from_le_bytes(&b)) {
        return Some(g);
    }
    let mut session = 0u32;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.is_ok() {
        let key = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\SessionInfo\{session}\VirtualDesktops");
        if let Some(g) = read_guid_value(&key, "CurrentVirtualDesktop").and_then(|b| guid_from_le_bytes(&b)) {
            return Some(g);
        }
    }
    read_guid_value(VD_KEY, "VirtualDesktopIDs").and_then(|b| guid_from_le_bytes(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_formatting() {
        let g = GUID::from_u128(0x6ad31dde_e515_4558_9d15_21dbe1193f72);
        assert_eq!(format_guid(&g), "{6AD31DDE-E515-4558-9D15-21DBE1193F72}");
        assert_eq!(format_guid(&GUID::zeroed()), NULL_GUID);
    }
}
