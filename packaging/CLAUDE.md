# packaging/CLAUDE.md

Details for the systemd units and OpenRC scripts. Claude Code loads this file once it reads or edits a file in this
directory; the repository-wide conventions are in the root `CLAUDE.md`.

## Path invariants (full constraints)

| Identifier | Location | Constraint |
|---|---|---|
| Install paths `/opt`, `/etc`, `/run/skyline-speeder` | config, units (the two OpenRC scripts in `packaging/openrc/` included), scripts | All three must agree |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **Must be the parent directory of `socket_path`** (and of `events_path`, `state_path`): the unit sets `ProtectSystem=strict`, under which this is the one directory in `/run` the daemon can write |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC's `RuntimeDirectory=`: **must be the parent directory of `socket_path`** (and of `events_path`, `state_path`), where the daemon opens its event log first |
| `StateDirectory=skyline-speeder`, `checkpath -d /var/lib/skyline-speeder` | `packaging/skyline-speederd.service`, `packaging/openrc/skyline-speederd` | **Must be the parent directory of `DAEMON_REPORTS_DIR`** (`/var/lib/skyline-speeder/reports`, `crates/skyline-common`), which `ssctl` and `install.sh` (`FEEDBACK_DAEMON_REPORTS`, and `--uninstall`, which removes it) read too: under `ProtectSystem=strict` it is the one directory under `/var/lib` the daemon can write, and a mismatch only costs a line on stderr when a report cannot be written -- the error report is silently lost |

> A bulk rename is the usual way to break the `RuntimeDirectory` / `socket_path` coupling (say,
> `RuntimeDirectory=skyline` while the config still says `/run/skyline-speeder/`). The daemon creates the parent
> directories of its event log, socket and state file itself (`Daemon::new`, `serve` and `persist_state` in `main.rs`),
> but `ProtectSystem=strict` leaves all of `/run` except `RuntimeDirectory=` read-only, so a mismatch makes it fail
> when it opens the event log, and systemd restarts it over and over. If you touch either side, check the other.

## The daemon's sandbox

`skyline-speederd.service` sets `NoNewPrivileges=true`, `UMask=0077`, `ProtectHome=true`, `RestrictSUIDSGID=`,
`LockPersonality=`, `RestrictRealtime=` and `SystemCallArchitectures=native` beside `ProtectSystem=strict` and
`PrivateTmp=`. Do not add `ProtectKernelTunables=` (the daemon writes `net.*` sysctls), `ProtectKernelModules=` (it
reads `/lib/modules`), `ProtectControlGroups=` (the `ExecStartPre=` mkdir and the cgroup attach) or
`PrivateNetwork=`, and check every new path the daemon writes against `ProtectSystem=strict` and `ProtectHome=`.
The OpenRC script sets only `umask`: its `no_new_privs` reached supervise-daemon as an option it does not take from
OpenRC 0.45 to 0.51, and the service would not start.

## The enable unit

`skyline-speeder-enable.service` (`openrc/skyline-speeder-enable` under OpenRC) exists to remove the "`enable` never
run" silent failure; do not delete it (root `CLAUDE.md`, "Silent failures", item 2). Its `ExecStop`
(`infra/boot-disable.sh`) is one of the writers of `fallback_cc`; the order and its preconditions are in
`crates/CLAUDE.md`, "Global sysctls".

This directory ships in the release artifact, so a change here needs a release (root `CLAUDE.md`, "Every change",
"Does it need a release?").
