# Contributing to fz-maximize

Thanks for helping! Bug reports, ideas and pull requests are all welcome.

## Reporting a bug

Open an issue using the bug report form. The most useful things to include:

* the output of `fz-maximize --dump` (monitors, matched FancyZones layout, zones);
* the relevant part of the log (`%LOCALAPPDATA%\fz-maximize\fz-maximize.log`, ideally with
  `log_level = "debug"` in Settings);
* your Windows and PowerToys versions, and which app/window misbehaved.

## Workflow: branches and pull requests

`main` is protected. Every change goes through a pull request:

1. Branch from `main`: `feature/<name>`, `fix/<name>`, or `chore/<name>`.
2. Push the branch and open a PR. Fill in the template.
3. **CI must pass** (the `check` job: fmt, clippy, unit tests), and the branch must be up
   to date with `main`.
4. **One approving review** is required. All review conversations must be resolved.
5. PRs are **squash-merged**, so `main` stays a linear list of one commit per change. The
   branch is deleted after merging.

Direct pushes, force-pushes and deleting `main` are blocked for everyone. Release tags
(`v*`) can only be created by maintainers and can't be moved or deleted once pushed.

Dependabot opens weekly PRs for Rust crates and GitHub Actions, plus immediate ones for
security advisories. Review them like any other PR.

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
