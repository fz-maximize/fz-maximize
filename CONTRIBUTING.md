# Contributing to fz-maximize

Thanks for helping! Bug reports, ideas and pull requests are all welcome.

## Reporting a bug

Open an issue using the bug report form. The most useful things to include:

* the output of `fz-maximize --dump` (monitors, matched FancyZones layout, zones);
* the relevant part of the log (`%LOCALAPPDATA%\fz-maximize\fz-maximize.log`, ideally with
  `log_level = "debug"` in Settings);
* your Windows and PowerToys versions, and which app/window misbehaved.

## Development

Prerequisites: Rust (installed automatically from `rust-toolchain.toml` by rustup), the
Visual Studio Build Tools (C++), and [Task](https://taskfile.dev). See the README.

```powershell
task            # list tasks
task check      # fmt + clippy (warnings are errors) + unit tests: must pass
task run        # run the debug build with logs in the terminal
task e2e        # live desktop tests (opens Notepad and drives it; don't touch the PC meanwhile)
```

Guidelines:

* Keep logic **pure and unit tested** where possible (`engine.rs`, `geometry.rs`,
  `fancyzones/`, `policy.rs`, `config.rs`, `preempt.rs` recognizer). Win32 calls belong in
  the thin platform modules.
* Behavior that has to match FancyZones should follow the upstream code. If you change a
  ported part, note which PowerToys file it mirrors (`upstream/fancyzones-baseline.json`).
* Run `cargo fmt` and keep `task check` green.
* Describe *why* in commit messages and PRs, not just what.

## License

By contributing, you agree that your contributions are licensed under the project's
[MIT License](LICENSE).
