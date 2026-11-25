# Security policy

## Reporting a vulnerability

Please **don't** open a public issue for security problems. Report them privately with
GitHub's **"Report a vulnerability"** button on this repository's **Security** tab
(private vulnerability reporting). We'll acknowledge the report, investigate and
coordinate a fix and disclosure with you.

## Scope

fz-maximize runs unelevated as the signed-in user. It installs low-level mouse and
keyboard hooks (in its own process; it never injects into other processes), reads
PowerToys/FancyZones settings files, and moves other windows. Relevant reports include,
for example, anything that lets another process or file influence fz-maximize into
misbehaving, leaking input, or running code.

## Supported versions

Only the latest release receives fixes.
