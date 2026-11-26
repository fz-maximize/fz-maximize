//! Update check and install (opt-in: setting `check_for_updates`, off by default; the
//! user is asked once).
//!
//! Once a day, fz-maximize asks GitHub for the project's latest release
//! (`api.github.com/repos/fz-maximize/fz-maximize/releases/latest`). When it's newer, a
//! notification offers it; the update window (`update_dialog.rs`) installs it now, at a
//! chosen time, reminds later or skips that version.
//!
//! Installing downloads the release's file for this copy (the installer, or the
//! portable zip), only from this project's GitHub release assets, checks its SHA-256
//! against the release's `SHA256SUMS.txt`, then runs the installer silently (which closes
//! and restarts fz-maximize) or, for a portable copy, swaps in the new exe and restarts.
//!
//! Network access goes through WinHTTP, so Windows' proxy settings apply.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const LATEST_URL: &str = "https://api.github.com/repos/fz-maximize/fz-maximize/releases/latest";
/// Release files must come from here (the project's own release assets).
const ASSET_PREFIX: &str = "https://github.com/fz-maximize/fz-maximize/releases/download/";
/// How often to check, when checking is on.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Downloads bigger than this are refused (releases are a few MB).
const MAX_DOWNLOAD: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------------------
// Pure logic (unit tested)
// ---------------------------------------------------------------------------------------

/// A release version, `major.minor.patch`. Pre-releases (`0.4.0-beta.1`) aren't offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// `"0.3.5"` or `"v0.3.5"`; `None` for anything else, including pre-releases.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let v = Version {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(v)
    }

    /// This build's version. For testing updates against a real release, the
    /// `FZM_UPDATE_TEST_CURRENT` environment variable (e.g. `0.3.0`) overrides it for
    /// the update check only.
    pub fn current() -> Self {
        std::env::var("FZM_UPDATE_TEST_CURRENT")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_else(|| Self::parse(env!("CARGO_PKG_VERSION")).expect("Cargo.toml version is major.minor.patch"))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release's page (release notes).
    pub notes_url: String,
    pub assets: Vec<Asset>,
}

/// Parse GitHub's "latest release" JSON. `None` for drafts, pre-releases and tags that
/// aren't plain versions.
pub fn parse_release(json: &str) -> Option<Release> {
    #[derive(Deserialize)]
    struct Gh {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        draft: bool,
        #[serde(default)]
        prerelease: bool,
        #[serde(default)]
        assets: Vec<GhAsset>,
    }
    #[derive(Deserialize)]
    struct GhAsset {
        name: String,
        browser_download_url: String,
    }
    let gh: Gh = serde_json::from_str(json).ok()?;
    if gh.draft || gh.prerelease {
        return None;
    }
    Some(Release {
        version: Version::parse(&gh.tag_name)?,
        notes_url: gh.html_url,
        assets: gh
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

/// How this copy of fz-maximize was installed, which decides how to update it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// By the installer: run the new installer. `all_users` installs need admin rights.
    Installer { all_users: bool },
    /// Extracted from the portable zip: replace this exe.
    Portable { exe: PathBuf },
    /// A development build (`target\...`): only tell about new versions.
    Dev,
}

/// The file to download for `kind` on `arch` ("x64" or "arm64"), and the checksums
/// file, both from this project's release assets.
pub fn pick_assets<'a>(release: &'a Release, kind: &InstallKind, arch: &str) -> Option<(&'a Asset, &'a Asset)> {
    let v = release.version;
    let wanted = match kind {
        InstallKind::Installer { .. } => format!("fz-maximize-{v}-{arch}-setup.exe"),
        InstallKind::Portable { .. } => format!("fz-maximize-{v}-{arch}-portable.zip"),
        InstallKind::Dev => return None,
    };
    let find = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name && a.url.starts_with(ASSET_PREFIX))
    };
    Some((find(&wanted)?, find("SHA256SUMS.txt")?))
}

/// The SHA-256 (lowercase hex) listed for `file` in a `sha256sum`-style list.
pub fn checksum_for(sums: &str, file: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == file && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    pub notes_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timed {
    pub version: String,
    pub at_unix: u64,
}

/// Persisted state (`%LOCALAPPDATA%\fz-maximize\update.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// The user has been asked whether to check for updates.
    pub asked: bool,
    pub last_check_unix: u64,
    /// A newer release found by the last check.
    pub available: Option<Pending>,
    /// A version the user chose to skip.
    pub skipped: Option<String>,
    /// Don't remind about `version` before `at_unix`.
    pub remind: Option<Timed>,
    /// Install `version` at `at_unix` (or as soon as fz-maximize runs after that).
    pub scheduled: Option<Timed>,
    /// An install of this version was started; reported on the next start.
    pub installing: Option<String>,
}

/// What to do about a release a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// Not newer than this version.
    UpToDate,
    /// Newer, and the user wants to hear about it now.
    Notify,
    /// Newer, but skipped, scheduled or snoozed: stay quiet.
    Quiet,
}

impl UpdateState {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    pub fn check_due(&self, now: u64) -> bool {
        self.last_check_unix == 0
            || now >= self.last_check_unix + CHECK_INTERVAL.as_secs()
            || now < self.last_check_unix
    }

    /// Record a check's result.
    pub fn found(&mut self, release: Option<&Release>, current: Version, now: u64) -> Found {
        self.last_check_unix = now;
        let Some(r) = release.filter(|r| r.version > current) else {
            self.available = None;
            return Found::UpToDate;
        };
        let v = r.version.to_string();
        self.available = Some(Pending {
            version: v.clone(),
            notes_url: r.notes_url.clone(),
        });
        let skipped = self.skipped.as_deref() == Some(&v);
        let scheduled = self.scheduled.as_ref().is_some_and(|s| s.version == v);
        let snoozed = self.remind.as_ref().is_some_and(|s| s.version == v && now < s.at_unix);
        if skipped || scheduled || snoozed {
            Found::Quiet
        } else {
            Found::Notify
        }
    }

    /// The available version, unless the user skipped it.
    pub fn offered(&self) -> Option<&Pending> {
        self.available
            .as_ref()
            .filter(|a| self.skipped.as_deref() != Some(a.version.as_str()))
    }

    pub fn skip(&mut self, version: &str) {
        self.skipped = Some(version.to_string());
        self.remind = None;
        self.scheduled = None;
    }

    pub fn remind_in(&mut self, version: &str, days: u64, now: u64) {
        self.remind = Some(Timed {
            version: version.to_string(),
            at_unix: now + days * 24 * 60 * 60,
        });
        self.scheduled = None;
    }

    pub fn schedule(&mut self, version: &str, at_unix: u64) {
        self.scheduled = Some(Timed {
            version: version.to_string(),
            at_unix,
        });
        self.remind = None;
    }

    /// A reminder whose time has come: clears it and returns its version.
    pub fn take_due_reminder(&mut self, now: u64) -> Option<String> {
        if self.remind.as_ref().is_some_and(|r| now >= r.at_unix) {
            return self.remind.take().map(|r| r.version);
        }
        None
    }

    /// A scheduled install whose time has come (it stays scheduled until it starts).
    pub fn due_install(&self, now: u64) -> Option<&str> {
        self.scheduled
            .as_ref()
            .filter(|s| now >= s.at_unix)
            .map(|s| s.version.as_str())
    }

    /// After an install was started: forget the offer and remember to report.
    pub fn started_install(&mut self, version: &str) {
        self.installing = Some(version.to_string());
        self.scheduled = None;
        self.remind = None;
    }

    /// On start: the version an install was started for, if any (cleared).
    pub fn take_install_result(&mut self) -> Option<String> {
        self.installing.take()
    }
}

/// Seconds from now until the next `target_minute` of the day (0..1440), given the
/// local time of day now in seconds. Always in the future: the same minute means
/// tomorrow.
pub fn seconds_until(now_local_secs: u32, target_minute: u32) -> u64 {
    const DAY: u32 = 24 * 60 * 60;
    let now = now_local_secs % DAY;
    let target = (target_minute % (24 * 60)) * 60;
    let delta = (target + DAY - now) % DAY;
    u64::from(if delta == 0 { DAY } else { delta })
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "x64" or "arm64": which build this is.
pub fn this_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" }
}

// ---------------------------------------------------------------------------------------
// Platform: network, hashing, installing
// ---------------------------------------------------------------------------------------

mod net {
    use windows::Win32::Networking::WinHttp::{
        INTERNET_DEFAULT_HTTPS_PORT, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
        WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
        WinHttpSendRequest, WinHttpSetTimeouts,
    };
    use windows::core::{HSTRING, PCWSTR, w};

    struct Handle(*mut core::ffi::c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }

    fn open(h: *mut core::ffi::c_void, what: &str) -> Result<Handle, String> {
        if h.is_null() {
            Err(format!("{what}: {}", windows::core::Error::from_thread()))
        } else {
            Ok(Handle(h))
        }
    }

    /// GET an `https://` URL; the body, if the status is 200. Redirects are followed.
    pub fn get(url: &str, accept: &str, limit: usize) -> Result<Vec<u8>, String> {
        let rest = url.strip_prefix("https://").ok_or("only https:// URLs")?;
        let (host, path) = rest
            .split_once('/')
            .map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
        let agent = HSTRING::from(format!("fz-maximize/{}", env!("CARGO_PKG_VERSION")));
        unsafe {
            let session = open(
                WinHttpOpen(
                    PCWSTR(agent.as_ptr()),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                ),
                "WinHttpOpen",
            )?;
            // Resolve, connect, send, receive (ms).
            let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
            let host = HSTRING::from(host);
            let conn = open(
                WinHttpConnect(session.0, PCWSTR(host.as_ptr()), INTERNET_DEFAULT_HTTPS_PORT, 0),
                "WinHttpConnect",
            )?;
            let path = HSTRING::from(path);
            let req = open(
                WinHttpOpenRequest(
                    conn.0,
                    w!("GET"),
                    PCWSTR(path.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    WINHTTP_FLAG_SECURE,
                ),
                "WinHttpOpenRequest",
            )?;
            let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
            WinHttpSendRequest(req.0, Some(&headers), None, 0, 0, 0).map_err(|e| format!("send: {e}"))?;
            WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("receive: {e}"))?;
            let mut status: u32 = 0;
            let mut len = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut len,
                std::ptr::null_mut(),
            )
            .map_err(|e| format!("status: {e}"))?;
            if status != 200 {
                return Err(format!("HTTP {status}"));
            }
            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                WinHttpQueryDataAvailable(req.0, &mut available).map_err(|e| format!("read: {e}"))?;
                if available == 0 {
                    break;
                }
                let start = body.len();
                body.resize(start + available as usize, 0);
                let mut read = 0u32;
                WinHttpReadData(req.0, body[start..].as_mut_ptr().cast(), available, &mut read)
                    .map_err(|e| format!("read: {e}"))?;
                body.truncate(start + read as usize);
                if body.len() > limit {
                    return Err("download too large".into());
                }
            }
            Ok(body)
        }
    }
}

/// SHA-256 of `data`, lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    if status.is_err() {
        return String::new();
    }
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ask GitHub for the latest release. `Ok(None)` when it isn't a plain release.
pub fn fetch_latest() -> Result<Option<Release>, String> {
    let body = net::get(LATEST_URL, "application/vnd.github+json", 1024 * 1024)?;
    Ok(parse_release(&String::from_utf8_lossy(&body)))
}

/// Download `asset` and check it against the release's checksums.
fn download_verified(asset: &Asset, sums: &Asset) -> Result<Vec<u8>, String> {
    let list = net::get(&sums.url, "application/octet-stream", 64 * 1024)?;
    let expected = checksum_for(&String::from_utf8_lossy(&list), &asset.name)
        .ok_or_else(|| format!("{} isn't listed in SHA256SUMS.txt", asset.name))?;
    let data = net::get(&asset.url, "application/octet-stream", MAX_DOWNLOAD)?;
    let actual = sha256_hex(&data);
    if actual != expected {
        return Err(format!("checksum mismatch for {} (got {actual})", asset.name));
    }
    Ok(data)
}

fn uninstall_location(root: windows::Win32::System::Registry::HKEY) -> Option<PathBuf> {
    use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::{HSTRING, PCWSTR};
    let key = HSTRING::from(
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{23A7EC64-695C-4A8E-A06E-FEBBD469AA50}_is1",
    );
    let value = HSTRING::from("InstallLocation");
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            root,
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
    let s = crate::win::from_wide(&buf[..(size as usize / 2)]);
    (!s.trim().is_empty()).then(|| PathBuf::from(s.trim()))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    norm(a) == norm(b)
}

/// How this copy was installed.
pub fn install_kind() -> InstallKind {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Dev;
    };
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let lower = exe.to_string_lossy().to_lowercase();
    if lower.contains(r"\target\") {
        return InstallKind::Dev;
    }
    for (root, all_users) in [(HKEY_CURRENT_USER, false), (HKEY_LOCAL_MACHINE, true)] {
        if uninstall_location(root).is_some_and(|loc| same_dir(&loc, &dir)) {
            return InstallKind::Installer { all_users };
        }
    }
    InstallKind::Portable { exe }
}

fn update_dir() -> PathBuf {
    std::env::temp_dir().join("fz-maximize-update")
}

/// What happened when an install was started.
pub enum Started {
    /// The installer is running; it closes this copy and starts the new one.
    Installer,
    /// The new exe is in place and was started; this copy should exit now.
    Restarting,
}

/// Download, verify and start installing `release` for this copy. `interactive`: show
/// the installer's progress (an install the user just asked for) rather than nothing (a
/// scheduled one). Runs on a worker thread.
pub fn install(release: &Release, interactive: bool) -> Result<Started, String> {
    let kind = install_kind();
    let (asset, sums) = pick_assets(release, &kind, this_arch()).ok_or_else(|| {
        format!(
            "no {} download for this copy in release {}",
            this_arch(),
            release.version
        )
    })?;
    let data = download_verified(asset, sums)?;
    let dir = update_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = dir.join(&asset.name);
    std::fs::write(&file, &data).map_err(|e| format!("{}: {e}", file.display()))?;
    match kind {
        InstallKind::Installer { all_users } => {
            let mut args = String::from(if interactive {
                "/SP- /SILENT"
            } else {
                "/SP- /VERYSILENT"
            });
            args.push_str(" /SUPPRESSMSGBOXES /NORESTART");
            if all_users {
                args.push_str(" /ALLUSERS");
            }
            shell_open(&file, &args, all_users)?;
            Ok(Started::Installer)
        }
        InstallKind::Portable { exe } => {
            replace_portable(&file, &dir, &exe)?;
            Ok(Started::Restarting)
        }
        InstallKind::Dev => unreachable!("pick_assets has no download for dev builds"),
    }
}

fn shell_open(file: &Path, args: &str, elevate: bool) -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};
    let file_w = HSTRING::from(file.as_os_str());
    let args_w = HSTRING::from(args);
    let verb = if elevate { w!("runas") } else { w!("open") };
    let r = unsafe {
        ShellExecuteW(
            None,
            verb,
            PCWSTR(file_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns a value > 32 on success.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("couldn't start {} (error {})", file.display(), r.0 as isize))
    }
}

/// Extract the new exe from the portable zip and swap it in for `exe`, then start it.
/// The running exe can be renamed (not overwritten), so it becomes `fz-maximize.exe.old`,
/// which the new copy deletes once this one has exited.
fn replace_portable(zip: &Path, dir: &Path, exe: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let tar = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
        .join("System32")
        .join("tar.exe");
    let out = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dir)
        .arg("fz-maximize.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("tar: {e}"))?;
    let new_exe = dir.join("fz-maximize.exe");
    if !out.status.success() || !new_exe.exists() {
        return Err(format!(
            "couldn't extract fz-maximize.exe: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("{}: {e}", exe.display()))?;
    if let Err(e) = std::fs::copy(&new_exe, exe) {
        // Put the old one back.
        let _ = std::fs::rename(&old, exe);
        return Err(format!("{}: {e}", exe.display()));
    }
    std::process::Command::new(exe)
        .arg("--after-update")
        .arg(std::process::id().to_string())
        .spawn()
        .map_err(|e| format!("couldn't start the new version: {e}"))?;
    Ok(())
}

/// Started by a portable update (`--after-update <pid>`): wait for the old copy to exit,
/// then remove its renamed exe.
pub fn finish_portable_update(old_pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, old_pid) {
            let _ = WaitForSingleObject(h, 15_000);
            let _ = CloseHandle(h);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}

/// Seconds since local midnight now.
pub fn local_seconds_of_day() -> u32 {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    u32::from(t.wHour) * 3600 + u32::from(t.wMinute) * 60 + u32::from(t.wSecond)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions() {
        assert_eq!(
            v("0.3.5"),
            Version {
                major: 0,
                minor: 3,
                patch: 5
            }
        );
        assert_eq!(
            v("v1.10.0"),
            Version {
                major: 1,
                minor: 10,
                patch: 0
            }
        );
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(Version::parse("0.4.0-beta.1").is_none());
        assert!(Version::parse("0.4").is_none());
        assert!(Version::parse("0.4.0.1").is_none());
        assert_eq!(v("v0.3.5").to_string(), "0.3.5");
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    const JSON: &str = r#"{
        "tag_name": "v0.3.5", "html_url": "https://github.com/fz-maximize/fz-maximize/releases/tag/v0.3.5",
        "draft": false, "prerelease": false,
        "assets": [
          {"name": "fz-maximize-0.3.5-x64-setup.exe", "browser_download_url": "https://github.com/fz-maximize/fz-maximize/releases/download/v0.3.5/fz-maximize-0.3.5-x64-setup.exe"},
          {"name": "fz-maximize-0.3.5-arm64-setup.exe", "browser_download_url": "https://github.com/fz-maximize/fz-maximize/releases/download/v0.3.5/fz-maximize-0.3.5-arm64-setup.exe"},
          {"name": "fz-maximize-0.3.5-x64-portable.zip", "browser_download_url": "https://github.com/fz-maximize/fz-maximize/releases/download/v0.3.5/fz-maximize-0.3.5-x64-portable.zip"},
          {"name": "SHA256SUMS.txt", "browser_download_url": "https://github.com/fz-maximize/fz-maximize/releases/download/v0.3.5/SHA256SUMS.txt"},
          {"name": "fz-maximize-0.3.5-arm64-portable.zip", "browser_download_url": "https://evil.example/fz-maximize-0.3.5-arm64-portable.zip"}
        ]}"#;

    #[test]
    fn releases_and_assets() {
        let r = parse_release(JSON).unwrap();
        assert_eq!(r.version, v("0.3.5"));
        assert!(r.notes_url.ends_with("/v0.3.5"));
        let installer = InstallKind::Installer { all_users: false };
        let (a, s) = pick_assets(&r, &installer, "x64").unwrap();
        assert_eq!(a.name, "fz-maximize-0.3.5-x64-setup.exe");
        assert_eq!(s.name, "SHA256SUMS.txt");
        let portable = InstallKind::Portable { exe: "x".into() };
        assert_eq!(
            pick_assets(&r, &portable, "x64").unwrap().0.name,
            "fz-maximize-0.3.5-x64-portable.zip"
        );
        assert!(
            pick_assets(&r, &portable, "arm64").is_none(),
            "only this project's release assets"
        );
        assert!(pick_assets(&r, &InstallKind::Dev, "x64").is_none());
        // Pre-releases, drafts and non-version tags aren't offered.
        assert!(parse_release(&JSON.replace(r#""prerelease": false"#, r#""prerelease": true"#)).is_none());
        assert!(parse_release(&JSON.replace(r#""draft": false"#, r#""draft": true"#)).is_none());
        assert!(parse_release(&JSON.replace("v0.3.5\", \"html", "nightly\", \"html")).is_none());
        assert!(parse_release("not json").is_none());
    }

    #[test]
    fn checksums() {
        let sums = "16ba635d8c63013af9a3a7e5567a400f17a4e266c223028c262460dc1b6be292  fz-maximize-0.3.4-arm64-portable.zip\n\
                    8665A79EBCD83D8FE3EAD02B58C933071D02525DA035C485BF24A155E25F0F4F *fz-maximize-0.3.4-x64-setup.exe\n";
        assert_eq!(
            checksum_for(sums, "fz-maximize-0.3.4-x64-setup.exe").as_deref(),
            Some("8665a79ebcd83d8fe3ead02b58c933071d02525da035c485bf24a155e25f0f4f")
        );
        assert!(checksum_for(sums, "fz-maximize-0.3.4-x64-portable.zip").is_none());
        assert!(checksum_for("abc  file\n", "file").is_none(), "not a SHA-256");
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn offers_skips_reminders_and_schedules() {
        let r = parse_release(JSON).unwrap();
        let cur = v("0.3.4");
        let mut s = UpdateState::default();
        assert!(s.check_due(1000));
        assert_eq!(s.found(Some(&r), cur, 1000), Found::Notify);
        assert!(!s.check_due(1000 + 60));
        assert!(s.check_due(1000 + CHECK_INTERVAL.as_secs()));
        assert_eq!(s.offered().unwrap().version, "0.3.5");

        // Remind in 3 days: quiet until then, then due once.
        s.remind_in("0.3.5", 3, 1000);
        assert_eq!(s.found(Some(&r), cur, 2000), Found::Quiet);
        assert_eq!(s.take_due_reminder(2000), None);
        assert_eq!(s.take_due_reminder(1000 + 3 * 86400).as_deref(), Some("0.3.5"));
        assert_eq!(s.found(Some(&r), cur, 1000 + 3 * 86400), Found::Notify);

        // Schedule: quiet; due at its time (or later), and cleared once started.
        s.schedule("0.3.5", 5000);
        assert_eq!(s.found(Some(&r), cur, 4000), Found::Quiet);
        assert_eq!(s.due_install(4999), None);
        assert_eq!(s.due_install(9999), Some("0.3.5"));
        s.started_install("0.3.5");
        assert_eq!(s.due_install(9999), None);
        assert_eq!(s.take_install_result().as_deref(), Some("0.3.5"));
        assert_eq!(s.take_install_result(), None);

        // Skip: quiet for that version, but a newer one is offered.
        s.skip("0.3.5");
        assert_eq!(s.found(Some(&r), cur, 6000), Found::Quiet);
        assert!(s.offered().is_none());
        let newer = parse_release(&JSON.replace("v0.3.5\", \"html", "v0.3.6\", \"html")).unwrap();
        assert_eq!(s.found(Some(&newer), cur, 7000), Found::Notify);

        // Up to date (or GitHub's latest is older): nothing available.
        assert_eq!(s.found(Some(&r), v("0.3.5"), 8000), Found::UpToDate);
        assert!(s.available.is_none());
        assert_eq!(s.found(None, cur, 9000), Found::UpToDate);
    }

    #[test]
    fn state_files_roundtrip_and_tolerate_old_or_broken_ones() {
        let dir = std::env::temp_dir().join(format!("fzm-update-test-{}", std::process::id()));
        let path = dir.join("update.json");
        let mut s = UpdateState {
            asked: true,
            ..Default::default()
        };
        s.schedule("0.3.5", 42);
        s.save(&path).unwrap();
        assert_eq!(UpdateState::load(&path), s);
        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(UpdateState::load(&path), UpdateState::default());
        assert_eq!(UpdateState::load(&dir.join("missing.json")), UpdateState::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scheduled_times_are_always_ahead() {
        let at = |h: u32, m: u32| h * 3600 + m * 60;
        // 22:00 now, 3:00 target: 5 hours.
        assert_eq!(seconds_until(at(22, 0), 3 * 60), 5 * 3600);
        // 1:30 now, 3:00 target: 1.5 hours.
        assert_eq!(seconds_until(at(1, 30), 3 * 60), 5400);
        // Exactly now: tomorrow.
        assert_eq!(seconds_until(at(3, 0), 3 * 60), 86400);
        // Seconds count: 2:59:30 -> 30 s.
        assert_eq!(seconds_until(at(2, 59) + 30, 3 * 60), 30);
    }

    #[test]
    fn install_locations_compare_loosely() {
        assert!(same_dir(
            Path::new(r"C:\Users\x\AppData\Local\Programs\fz-maximize\"),
            Path::new(r"c:\users\x\appdata\local\programs\FZ-MAXIMIZE")
        ));
        assert!(!same_dir(Path::new(r"C:\a"), Path::new(r"C:\b")));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "network: asks GitHub for the latest release and downloads it"]
    fn latest_release_downloads_and_verifies() {
        let r = fetch_latest().expect("GitHub reachable").expect("a plain release");
        eprintln!("latest: {} ({} assets)", r.version, r.assets.len());
        let kind = InstallKind::Installer { all_users: false };
        let (asset, sums) = pick_assets(&r, &kind, this_arch()).expect("installer + checksums");
        let data = download_verified(asset, sums).expect("download matches SHA256SUMS.txt");
        eprintln!("{}: {} bytes, sha256 {}", asset.name, data.len(), sha256_hex(&data));
        assert!(data.starts_with(b"MZ"), "an exe");
    }
}
