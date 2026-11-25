//! "Start with Windows" via `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteKeyValueW, RegGetValueW,
    RegOpenKeyExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "fz-maximize";

/// The command line stored in the Run key for `exe`.
pub fn command_for(exe: &std::path::Path) -> String {
    format!("\"{}\"", exe.display())
}

fn read() -> Option<String> {
    let key = HSTRING::from(RUN_KEY);
    let value = HSTRING::from(VALUE);
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    buf.truncate(size as usize / 2);
    Some(crate::win::from_wide(&buf))
}

/// True when the Run entry exists and points at `exe`.
pub fn is_enabled(exe: &std::path::Path) -> bool {
    read().is_some_and(|v| v.eq_ignore_ascii_case(&command_for(exe)))
}

pub fn enable(exe: &std::path::Path) -> windows::core::Result<()> {
    let key_name = HSTRING::from(RUN_KEY);
    let mut key = HKEY::default();
    unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut key,
        )
        .ok()?;
        let data: Vec<u16> = command_for(exe).encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len() * 2);
        let value = HSTRING::from(VALUE);
        let rc = RegSetValueExW(key, PCWSTR(value.as_ptr()), None, REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        rc.ok()
    }
}

pub fn disable() -> windows::core::Result<()> {
    let key = HSTRING::from(RUN_KEY);
    let value = HSTRING::from(VALUE);
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(value.as_ptr())) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    rc.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_quoted() {
        let p = std::path::Path::new(r"C:\Program Files\fz-maximize\fz-maximize.exe");
        assert_eq!(command_for(p), r#""C:\Program Files\fz-maximize\fz-maximize.exe""#);
    }
}
