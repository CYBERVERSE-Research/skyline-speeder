# packaging/CLAUDE.md

Details for the systemd units and OpenRC scripts. Claude Code loads this file once it reads or edits a file in this
directory; the repository-wide conventions are in the root `CLAUDE.md`.

## Path invariants (full constraints)

| Identifier | Location | Constraint |
|---|---|---|
| Install paths `/opt`, `/etc`, `/run/skyline-speeder` | config, units (the two OpenRC scripts in `packaging/openrc/` included), scripts | All three must agree |
| `RuntimeDirectory=skyline-speeder` | `packaging/skyline-speederd.service` | **Must be the parent directory of `socket_path`** (and of `events_path`, `state_path`): the unit sets `ProtectSystem=strict`, under which this is the one directory in `/run` the daemon can write |
| `checkpath -d /run/skyline-speeder` | `packaging/openrc/skyline-speederd` | OpenRC's `RuntimeDirectory=`: **must be the parent directory of `socket_path`** (and of `events_path`, `state_path`), where the daemon opens its event log first |

> The `RuntimeDirectory` / `socket_path` coupling is a hole this project has already fallen into once: a bulk rename
> changed `RuntimeDirectory` to `skyline` while the config still said `/run/skyline-speeder/`. The daemon started fine
> then and the control socket simply never appeared. Today's daemon creates the parent directories of its event log,
> socket and state file itself (`Daemon::new`, `serve` and `persist_state` in `main.rs`), and `ProtectSystem=strict`
> leaves all of `/run` but `RuntimeDirectory=` read-only, so, by the unit and the code, the same mismatch now makes it
> fail when it opens the event log and be restarted by systemd over and over: the invariant stands, the symptom
> changed. If you touch either side, check the other.

## The enable unit

`skyline-speeder-enable.service` (`openrc/skyline-speeder-enable` under OpenRC) exists to remove the "`enable` never
run" silent failure; do not delete it (root `CLAUDE.md`, "Silent failure points", item 2). Its `ExecStop`
(`infra/boot-disable.sh`) is one of the writers of `fallback_cc`; the order and its preconditions are in
`crates/CLAUDE.md`, "Global sysctls".

This directory ships in the release artifact, so a change here needs a release (root `CLAUDE.md`, "Prebuilt
releases").
