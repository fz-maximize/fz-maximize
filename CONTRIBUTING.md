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
5. `main` keeps a **linear history** of the PR's own commits (rebased onto `main`).

Force-pushes and deleting `main` are blocked for everyone. Direct pushes are blocked
except for maintainers merging a PR. Release tags (`v*`) can only be created by
maintainers and can't be moved or deleted once pushed.

### Merging (maintainers)

Don't use GitHub's merge button. Any merge GitHub performs (squash or rebase) creates new
commits on GitHub's servers and records the merging account's name and email as
committer. Instead, fast-forward `main` to the PR's commits, so `main` gets exactly the
commits that were reviewed and passed CI, with their own identity:

```powershell
git fetch origin
git switch <branch>
git rebase origin/main                 # if needed; push the branch and let CI pass again
git push origin <branch>:main          # fast-forward; GitHub marks the PR as merged
git push origin --delete <branch>
```

Maintainers commit with the project identity, not a personal one. In your clone:
`git config user.name fz-maximize` and
`git config user.email 361363014+fzmaximize@users.noreply.github.com` (the fzmaximize
account's private GitHub address; older commits use `maintainers@fz-maximize.invalid`).

Dependabot opens weekly PRs for Rust crates, GitHub Actions and the website's npm
packages, plus immediate ones for security advisories. Low-risk ones (website packages,
and patch or minor updates of crates and Actions) are approved by the
fz-maximize-automerge app and merge on their own once CI passes; see
`.github/workflows/dependabot-automerge.yml`. Review the rest (major versions, changed
maintainers) like any other PR.

## Development

Prerequisites, all tasks, the project layout and how fz-maximize stays in sync with
FancyZones are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md). In short:

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
* User-facing changes need their documentation updated: the website pages in
  `site/src/content/docs/` (preview with `task site`) and, if it's a headline feature, the
  README.

## License

By contributing, you agree that your contributions are licensed under the project's
[MIT License](LICENSE).
