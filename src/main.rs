//! fz-maximize: make the Windows maximize action fill the PowerToys FancyZones zone a
//! window is in, instead of the whole monitor.
//!
//! See `docs/IMPLEMENTATION_PLAN.md` for the design.

// Release builds have no console window; debug builds keep one for logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod animate;
mod app;
mod autostart;
mod config;
mod drift;
mod engine;
mod fancyzones;
mod filters;
mod geometry;
mod hooks;
mod logging;
mod monitors;
mod policy;
mod preempt;
mod settings;
mod tray;
mod update;
mod update_dialog;
mod vdesktop;
mod win;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
use windows::core::{HSTRING, PCWSTR, w};

const USAGE: &str = "\
fz-maximize - maximize windows into their PowerToys FancyZones zone

USAGE:
    fz-maximize [--console] [--config <path>]     run (tray icon)
    fz-maximize --dump [--json] [--config <path>] show monitors, layouts and zones
    fz-maximize --quit                            stop the running instance
    fz-maximize --settings                        open the settings window of the running instance
    fz-maximize --init-config [--config <path>]   create the config file (commented defaults)
    fz-maximize --autostart <on|off|status>       manage \"Start with Windows\"
    fz-maximize --version | --help

OPTIONS:
    --console        log to the console instead of the log file (debug level)
    --config <path>  use this config file instead of %APPDATA%\\fz-maximize\\config.toml
    --json           machine-readable --dump output

Holding Shift while maximizing (or pressing Win+Shift+Up) gives the normal full-monitor
maximize; the key is configurable.

Documentation: https://fz-maximize.github.io/fz-maximize/";

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Run { console: bool },
    Dump { json: bool },
    Quit,
    Settings,
    InitConfig,
    Autostart(String),
    Version,
    Help,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    cmd: Cmd,
    config: Option<PathBuf>,
    /// Started by a portable update: the process id of the copy it replaces.
    after_update: Option<u32>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut console = false;
    let mut json = false;
    let mut cmd: Option<Cmd> = None;
    let mut config = None;
    let mut after_update = None;
    let mut it = args.into_iter();
    let set = |c: Cmd, cmd: &mut Option<Cmd>| -> Result<(), String> {
        if cmd.is_some() {
            return Err("only one command may be given".into());
        }
        *cmd = Some(c);
        Ok(())
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--console" | "-c" => console = true,
            "--json" => json = true,
            "--config" => config = Some(PathBuf::from(it.next().ok_or("--config needs a path")?)),
            "--dump" | "dump" => set(Cmd::Dump { json: false }, &mut cmd)?,
            "--quit" | "quit" => set(Cmd::Quit, &mut cmd)?,
            "--init-config" => set(Cmd::InitConfig, &mut cmd)?,
            "--settings" | "settings" => set(Cmd::Settings, &mut cmd)?,
            "--autostart" => set(
                Cmd::Autostart(it.next().ok_or("--autostart needs on|off|status")?),
                &mut cmd,
            )?,
            "--after-update" => {
                after_update = Some(
                    it.next()
                        .and_then(|p| p.parse().ok())
                        .ok_or("--after-update needs a process id")?,
                )
            }
            "--version" | "-V" => set(Cmd::Version, &mut cmd)?,
            "--help" | "-h" | "/?" => set(Cmd::Help, &mut cmd)?,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    let cmd = match cmd {
        Some(Cmd::Dump { .. }) => Cmd::Dump { json },
        Some(c) => c,
        None => Cmd::Run { console },
    };
    Ok(Args {
        cmd,
        config,
        after_update,
    })
}

/// In release builds (GUI subsystem) attach to the parent console so CLI output shows,
/// but only when stdout isn't already redirected (pipe/file); attaching would otherwise
/// send the output to the console instead of the redirect.
fn attach_console() {
    unsafe {
        let redirected = GetStdHandle(STD_OUTPUT_HANDLE).is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
        if !redirected {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Print to stdout, ignoring a closed pipe (e.g. `fz-maximize --dump | head`) instead
/// of panicking like `println!`.
fn out(text: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{text}");
    let _ = stdout.flush();
}

fn message_box(text: &str) {
    let text = HSTRING::from(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("fz-maximize"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn main() -> ExitCode {
    // Must come first: every RECT we read or write is in physical pixels.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            attach_console();
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let config_path = args
        .config
        .clone()
        .or_else(config::default_config_path)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    match args.cmd {
        Cmd::Help => {
            attach_console();
            out(USAGE);
            ExitCode::SUCCESS
        }
        Cmd::Version => {
            attach_console();
            out(&format!("fz-maximize {}", env!("CARGO_PKG_VERSION")));
            ExitCode::SUCCESS
        }
        Cmd::Dump { json } => {
            attach_console();
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            match app::dump(&config_path, json) {
                Ok(s) => {
                    out(&s);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Quit => {
            attach_console();
            if app::signal_quit() {
                println!("asked the running fz-maximize to exit");
                ExitCode::SUCCESS
            } else {
                println!("fz-maximize is not running");
                ExitCode::from(1)
            }
        }
        Cmd::Settings => {
            attach_console();
            if app::signal_open_settings() {
                out("opened the settings window of the running fz-maximize");
                ExitCode::SUCCESS
            } else {
                eprintln!("fz-maximize is not running; start it first");
                ExitCode::from(1)
            }
        }
        Cmd::InitConfig => {
            attach_console();
            let existed = config_path.exists();
            match config::ensure_exists(&config_path) {
                Ok(()) => {
                    println!(
                        "{} {}",
                        if existed { "config already exists:" } else { "created" },
                        config_path.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {}: {e}", config_path.display());
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Autostart(mode) => {
            attach_console();
            let exe = std::env::current_exe().unwrap_or_default();
            let result = match mode.as_str() {
                "on" => autostart::enable(&exe).map(|_| "enabled"),
                "off" => autostart::disable().map(|_| "disabled"),
                "status" => Ok(if autostart::is_enabled(&exe) {
                    "enabled"
                } else {
                    "disabled"
                }),
                other => {
                    eprintln!("error: --autostart expects on|off|status, got '{other}'");
                    return ExitCode::from(2);
                }
            };
            match result {
                Ok(state) => {
                    println!("start with Windows: {state} ({})", exe.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Cmd::Run { console } => {
            if let Some(pid) = args.after_update {
                // The old copy exits right after starting this one; let it go first.
                update::finish_portable_update(pid);
            }
            run(config_path, console)
        }
    }
}

fn run(config_path: PathBuf, console: bool) -> ExitCode {
    if console {
        attach_console();
    }
    let (cfg, config_error) = match config::load(&config_path) {
        Ok(c) => (c, None),
        Err(e) => (config::Config::default(), Some(e)),
    };
    let level = if console && std::env::var_os("FZM_LOG").is_none() {
        "debug".to_string()
    } else {
        cfg.log_level.clone()
    };
    logging::init(&level, console);
    tracing::info!(version = env!("CARGO_PKG_VERSION"), config = %config_path.display(), "fz-maximize starting");
    if let Some(e) = &config_error {
        tracing::error!(error = %e, "config error; using defaults");
    }

    // Single instance per session. The handle stays open for the process lifetime.
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\fz-maximize.single-instance")) };
    if mutex.is_err() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        tracing::warn!("another instance is already running");
        if console {
            eprintln!("fz-maximize is already running (use --quit to stop it)");
        } else {
            message_box("fz-maximize is already running.\n\nUse its tray icon to pause or exit it.");
        }
        return ExitCode::from(1);
    }

    // MTA: COM calls (virtual desktop manager) then never pump our message queue, so
    // WinEvent callbacks cannot re-enter while one is in progress.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    match app::run(config_path, cfg, config_error) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "fatal");
            if !console {
                message_box(&format!("fz-maximize failed to start:\n{e:#}"));
            } else {
                eprintln!("fatal: {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn cli_parsing() {
        assert_eq!(p(&[]).unwrap().cmd, Cmd::Run { console: false });
        assert_eq!(p(&["--console"]).unwrap().cmd, Cmd::Run { console: true });
        assert_eq!(p(&["--dump", "--json"]).unwrap().cmd, Cmd::Dump { json: true });
        assert_eq!(p(&["--json", "--dump"]).unwrap().cmd, Cmd::Dump { json: true });
        assert_eq!(p(&["--dump"]).unwrap().cmd, Cmd::Dump { json: false });
        assert_eq!(p(&["--autostart", "on"]).unwrap().cmd, Cmd::Autostart("on".into()));
        assert_eq!(p(&["--init-config"]).unwrap().cmd, Cmd::InitConfig);
        assert_eq!(p(&["--settings"]).unwrap().cmd, Cmd::Settings);
        let a = p(&["--config", r"C:\x\c.toml"]).unwrap();
        assert_eq!(a.config, Some(PathBuf::from(r"C:\x\c.toml")));
        assert!(p(&["--config"]).is_err());
        assert!(p(&["--dump", "--quit"]).is_err());
        assert!(p(&["--bogus"]).is_err());
    }
}
