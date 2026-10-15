# docs/CLAUDE.md

The documentation map. Claude Code loads this file once it reads or edits a file in this directory. Which documents a
change must also update is in the root `CLAUDE.md`, "Documentation sync"; the bar for performance numbers is in its
"Performance-claim discipline".

| Document | Reader and content |
|---|---|
| `README.md` / `README.zh.md` | Users, in the order a newcomer needs it: what it is → the kernel requirement → quick start → how it works and how the three algorithms differ → configuration (what a default install runs, a subsection per feature, and what every line of `ssctl status` and `ssctl flows` means) → layout → building from source → documentation → measured performance → contributing → license. The English and Chinese READMEs say the same thing |
| `DEPLOY.md` | **The one deployment and operations manual** (Chinese), written for automation agents and followed by people: prerequisites, every installer step, verification gates, the cgroup, the guard, upgrades, removal, troubleshooting |
| `docs/usage.md` | Newcomers (Chinese): every switch and parameter, four presets, dynamic RTO, DSCP, finding the cause of a stalled connection |
| `docs/02-interface-reference.md` | `ssctl` commands, the wire protocol, status fields and counters, ABI versions, configuration fields, event codes, guard rules |
| `docs/03-design.md` | How each module works, the trade-offs, open questions |
| `docs/04-performance-report.md` | The two-VM test bed's data, what it does and does not show, its limits |
| `research/experiments/README.md` | The experiment harness and how to reproduce a run |
| `research/experiments/single-ended/README.md`, `RESULTS.md` | The one-path field comparison of bbr, tcp-brutal and skyline_cc: method, per-run data, raw records and the scripts to reproduce it; the README's measured-performance figures and numbers come from here. Not test-bed data, and never in `docs/04-performance-report.md` |
| `CHANGELOG.md` | Every release and its *Upgrading from ...* notes |
| `CONTRIBUTING.md` | The conventions of the `CLAUDE.md` files, for human contributors |
| `CLAUDE.md` and the `CLAUDE.md` in `bpf/`, `crates/`, `config/`, `packaging/`, `infra/` and `docs/` | AI coding agents: the root file is loaded in every session and holds the repository-wide rules and an index; each directory's file is loaded once an agent reads or edits a file there and holds that directory's details |
