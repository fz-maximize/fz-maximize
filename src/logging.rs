//! Logging: a size-capped file in `%LOCALAPPDATA%\fz-maximize` (or stderr with
//! `--console`), with a runtime-adjustable level.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::reload;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

type ReloadHandle = reload::Handle<LevelFilter, tracing_subscriber::Registry>;
static HANDLE: OnceLock<ReloadHandle> = OnceLock::new();

/// `%LOCALAPPDATA%\fz-maximize`
pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("fz-maximize"))
}

pub fn log_file() -> Option<PathBuf> {
    log_dir().map(|d| d.join("fz-maximize.log"))
}

pub fn parse_level(s: &str) -> Option<LevelFilter> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::OFF,
        "error" => LevelFilter::ERROR,
        "warn" => LevelFilter::WARN,
        "info" => LevelFilter::INFO,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        _ => return None,
    })
}

/// `FZM_LOG` overrides the configured level.
fn effective_level(configured: &str) -> LevelFilter {
    std::env::var("FZM_LOG")
        .ok()
        .and_then(|v| parse_level(&v))
        .or_else(|| parse_level(configured))
        .unwrap_or(LevelFilter::INFO)
}

/// Keep one previous log (`.log.old`) once the current one exceeds the cap.
fn open_rotated(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    OpenOptions::new().create(true).append(true).open(path)
}

/// Initialise logging once. `console` logs to stderr instead of the file.
pub fn init(configured_level: &str, console: bool) {
    let (filter, handle) = reload::Layer::new(effective_level(configured_level));
    let _ = HANDLE.set(handle);
    let registry = tracing_subscriber::registry().with(filter);

    let file = if console {
        None
    } else {
        log_file().and_then(|p| open_rotated(&p).ok())
    };
    match file {
        Some(f) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(Mutex::new(f));
            let _ = registry.with(layer).try_init();
        }
        None => {
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(std::io::stderr);
            let _ = registry.with(layer).try_init();
        }
    }
}

/// Apply a new configured level (after a config reload).
pub fn set_level(configured_level: &str) {
    if let Some(h) = HANDLE.get() {
        let _ = h.modify(|f| *f = effective_level(configured_level));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(parse_level("INFO"), Some(LevelFilter::INFO));
        assert_eq!(parse_level(" debug "), Some(LevelFilter::DEBUG));
        assert_eq!(parse_level("verbose"), None);
    }

    #[test]
    fn rotation() {
        let dir = std::env::temp_dir().join(format!("fzm-log-test-{}", std::process::id()));
        let path = dir.join("fz-maximize.log");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        drop(open_rotated(&path).unwrap());
        assert!(path.with_extension("log.old").exists());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
