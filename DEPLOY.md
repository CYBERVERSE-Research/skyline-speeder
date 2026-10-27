# Skyline Speeder — Agent deployment manual

A deterministic deployment and operations manual for **automated agents and unattended scripts**, and the
**only** deployment document of this project: pre-deployment assessment, installation, verification, where to
tune, upgrades, troubleshooting and uninstallation are all here, and human operators follow it too. Every step
gives a success condition a machine can check, and what to do about the known failure modes.

- Tuning for newcomers, ready-made recipes, troubleshooting stalled connections: [docs/usage.md](docs/usage.md)
- The exact definition of every command, field and configuration key: [docs/02-interface-reference.md](docs/02-interface-reference.md)
- How the algorithm works: [docs/03-design.md](docs/03-design.md); performance data and where it applies: [docs/04-performance-report.md](docs/04-performance-report.md)

**Conventions**

- All commands run as root. As an ordinary user, prefix them with `sudo`; pipe the bootstrap script into
  `| sudo bash`, and hand environment variables to sudo itself: `sudo VAR=value ./install.sh`,
  `| sudo VAR=value bash`. Not `sudo -E`, which also gives root your `HOME`: a source build refuses a `HOME` that
  root does not own, since it would run the Rust toolchain from there.
- A non-zero exit code is a failure: do not go on to the next step. The exceptions are stated where they occur:
  an `ssctl drain` timeout (section 11.1) and the warnings of `--check` (section 2.2).
- Commands are written for systemd; for the OpenRC (Alpine) equivalents see the table in section 12.

Read values out of `ssctl status --json` with the function below; define it once in every new shell. It works
the way `install.sh` does: the JSON `ssctl` prints has one key per line, and the function takes the value of
the **first occurrence** of a key, without depending on python3 or jq (minimal images lack them).
`version` and `enabled` are the first two fields of `status`, and `armed` appears only inside `guard`, so the
first occurrence is exactly the one wanted. `ssctl` v0.2.0 and earlier does not know `--json` (it prints JSON
without arguments); the function then falls back to `ssctl status` without arguments:

```sh
sv() { { ssctl status --json 2>/dev/null || ssctl status 2>/dev/null; } \
         | awk -v k="\"$1\":" '!f && $1 == k { v = $2; gsub(/[",]/, "", v); print v; f = 1 }'; }
sv enabled    # true / false: whether skyline_cc is attached
sv version    # version of the running daemon; empty for daemons v0.2.0 and earlier, which do not report one
sv armed      # whether the guard is armed
```

> [!WARNING]
> Do not test whether it is attached with `ssctl status --json | grep -q '"enabled": true'`: since 0.4.0 the
> JSON also has `redundancy.config.enabled`, which defaults to `true`, so that grep succeeds even when nothing
> is attached.

---

## 0. Quick reference

### 0.1 Installation

```bash
# With a source tree (repository root)
./install.sh                  # default: install the latest published prebuilt artifact; build from source automatically when this host cannot run it (section 2.1)
./install.sh --source         # build from this source tree on this host

# Without a source tree: Debian/Ubuntu, Fedora and the RHEL family (without curl, first apt-get/dnf install -y curl)
curl -fsSL https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | bash
# Alpine: only busybox's wget, no curl and no bash; the bootstrap script adds them
wget -qO- https://raw.githubusercontent.com/CYBERVERSE-Research/skyline-speeder/main/scripts/bootstrap.sh | sh
```

`scripts/bootstrap.sh` puts the source tree at `/usr/local/src/skyline-speeder` (an existing old tree is moved
to `<dir>.bak-<timestamp>`, not deleted), installs whatever of `curl`, `bash` and `tar` is missing with the
host's package manager, and then runs the `install.sh` there. Other arguments are passed through unchanged:
`... | bash -s -- --source` (on Alpine, `... | sh -s -- --source`). The `install.sh` it runs comes from `--ref`
(default `main`), but the artifact it installs comes from the latest release (or the one `--release` names):
`--ref` chooses the installer, not the version installed. Later upgrades or an uninstall can use that tree
directly: `/usr/local/src/skyline-speeder/install.sh --uninstall`; when `--uninstall` is passed through the
bootstrap script, it also uses this tree directly and downloads nothing. Without that tree, `--uninstall` and
`--check` through the bootstrap script run the downloaded installer directly, and place or move no source tree.
Every upgrade through the bootstrap script leaves one more `<dir>.bak-<timestamp>`; when there is more than
one, it reminds you to clean them up (it never deletes anything itself).

### 0.2 Success criteria

```sh
v=$(skyline-speederd --version 2>/dev/null | awk '{print $2}')
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = skyline_cc ] \
  && [ "$(sv enabled)" = true ] \
  && { [ -z "$v" ] || [ "$(sv version)" = "$v" ]; }
```

The three conditions confirm, in order: the host-wide default algorithm is `skyline_cc`; the daemon itself
reports that it is attached (the sysctl alone is not enough: after the daemon is killed from outside, the
default can still point at a `skyline_cc` that has been unregistered); and what runs in memory is the version
just installed (`--version` reads the binary on disk, the `version` of `status` comes from the running process;
if they differ, the daemon has not been restarted into the new version yet). Binaries v0.2.0 and earlier have
no `--version`, and the third condition is skipped with it. These criteria do not apply to a `--no-enable`
install (section 2.2), which by design does not attach.

### 0.3 Output and exit codes

- When stdout is not a terminal (an agent, CI, a redirect to a file), each step prints one line when it starts
  and one when it ends, `[ n/N] <step name>` / `[ n/N] done in <elapsed>`; the last such line is
  `all N steps done in <elapsed>`, followed by the install summary and the usage guide. On a terminal it is a
  single progress bar refreshed in place. N is 9 (prebuilt) or 11 (source build), 2 fewer each with
  `--no-enable`.
- `--verbose` (or the environment variable `SKYLINE_VERBOSE=1`) draws no progress bar: step lines become
  `==> (n/N) <step name>`, there are no `done in` or `all N steps` lines, and the output of every subcommand
  also goes to the screen.
- `warn …` and `error …` go to stderr; progress and the summary go to stdout.
- All output of the package manager, rustup, make, cargo and the verifier goes to
  `/var/log/skyline-speeder-install.log`, which is emptied at the start of every install and kept afterwards
  (`--check` and `--uninstall` do not write it). When a step fails, the installer prints `error <reason>`, at
  most the last 25 lines of that step in the log, and the log path, and exits non-zero. Interrupted with
  Ctrl-C, it exits with 130, and background build processes end with it.
- After a failed install, upgrade or uninstall, the installer asks on the terminal (`/dev/tty`) whether to send
  an error report, and sends one only on `y` (section 13.6). **A run with no controlling terminal -- an agent, CI,
  cloud-init -- is never asked and sends nothing**; the exit code is the same either way. `SKYLINE_FEEDBACK=yes`
  sends without asking, `SKYLINE_FEEDBACK=no` never asks.
- Duration: a prebuilt install usually takes under a minute, plus up to 60 seconds of drain on an upgrade
  (section 10); a source build takes 5–10 minutes on a 1-vCPU host, longer on Alpine. Allow generous timeouts.

---

## 1. Pre-deployment assessment

### 1.1 Where it goes and what it changes

- **Deploy it only on the end that sends the data** (usually the server). Clients stay on standard TCP and
  need no change. So what it accelerates is the data this host **sends**: users' downloads get faster, uploads
  do not.
- `ssctl enable` (which the installer runs automatically) **takes effect host-wide**: from then on every new
  TCP connection on this host uses `skyline_cc` by default, not just some process (section 5.2).
- While attached, `skyline-speederd` exclusively owns three settings: the default congestion control,
  `net.core.default_qdisc` and the root qdisc of the egress NIC (section 9). The installer installs no proxy and
  opens no port.

### 1.2 Link suitability

> [!WARNING]
> The self-protection mechanisms recognize only two real congestion signals: growing queueing delay and ECN
> marks. They assume that **loss by itself does not mean congestion**. If the link's loss actually comes from
> queue overflow in a device, loss compensation and the queueing guardrail pull in opposite directions and only
> make things worse. Nor does it aim for fairness: it makes no deliberate effort to share the link evenly with
> competing flows.

How to tell: run `iperf3 -s` on the peer and send fixed-rate UDP from this host, stepping up the load:

```bash
for r in 100M 200M 400M 800M; do
    iperf3 -c <peer> -u -b $r -t 12 | tail -3
done
```

| Observation | Conclusion |
|---|---|
| UDP shows **zero loss at every rate**, while TCP retransmits heavily | The loss is queue overflow caused by its own bursts: **not suitable** |
| UDP has steady loss even at low rates | Random link-layer loss: suitable |
| No loss at low rates, loss only once the rate is high | The link is being filled: congestion loss, **not suitable** |

ICMP loss rate **cannot** serve as the criterion: most networks rate-limit ICMP, and it has nothing to do with
data-plane loss. For the other known validity boundaries and limitations see
`docs/04-performance-report.md` section 8.

### 1.3 Hard prerequisites

| Condition | Check command | Requirement |
|---|---|---|
| Kernel version | `uname -r` | **>= 6.12** (ABI minimum 6.10, see below) |
| Kernel BTF | `test -r /sys/kernel/btf/vmlinux` | Present (`CONFIG_DEBUG_INFO_BTF=y`) |
| cgroup v2 | `test -e /sys/fs/cgroup/cgroup.controllers` | Mounted unified at `/sys/fs/cgroup` (on OpenRC hosts the `cgroups` service mounts it, and the installer starts that service) |
| `fq` qdisc | `modprobe sch_fq` | Built in or loadable |
| struct_ops | `struct_ops` in the capability report of section 4 | `true` |
| `fallback_cc` | `fallback_cc_available` in the capability report of section 4 | `true` (section 8.1) |
| bpffs | `mount \| grep -q ' /sys/fs/bpf '` | Mounted (when it is not, the installer fails at the step that creates `/sys/fs/bpf/skyline-speeder`; `bpffs` in the capability report only checks that the directory `/sys/fs/bpf` exists — it exists even when bpffs is not mounted — so it cannot stand in for this check) |
| Distribution | `cat /etc/os-release` | Debian/Ubuntu (apt + systemd), Fedora and the RHEL family — Rocky Linux, AlmaLinux, CentOS Stream (dnf + systemd), Alpine (apk + OpenRC); other distributions are rejected at the first step with a non-zero exit code |

What the daemon itself enforces are six items of the capability report: `btf`, `bpffs`, `cgroup_v2`,
`fq_available`, `struct_ops`, `fallback_cc_available`. If any is `false`, `--validate-only`, `ssctl validate`
and `ssctl enable` all refuse outright rather than run with an incomplete capability set; the
`skyline-speederd.service` process itself still starts. The one-line install has run end to end on Debian 13,
Fedora 43, Rocky Linux 10.2 and Alpine 3.23.

> [!CAUTION]
> **The `6.1.x` and `6.6.x` LTS branches are explicitly unsupported.** The `cong_control` callback in
> `skyline_cc.bpf.c` is declared with 4 arguments (`sk, ack, flag, rs`), a signature that exists only from
> v6.10; earlier kernels have only the 2-argument version (`sk, rs`), and the verifier rejects the program
> outright at load time, with an error that points clearly at the argument-count mismatch. **This is not a
> configuration problem; do not try to work around it.**

`6.10`/`6.11` satisfy the ABI but are not LTS, and are not recommended as targets for long-term deployment;
what is verified and committed to is **6.12 LTS and later** (6.12, 6.18, 7.1 and others have passed in
testing). This restriction comes only from `skyline_cc`: `skyline_tc` and `skyline_policy` do not touch
`cong_control`, but releases are verified only against the overall 6.12 threshold.

---

## 2. One-line install: `install.sh`

### 2.1 Prebuilt artifact or source build

Without arguments, the installer first decides whether this host can run the released binaries: the
architecture is one the release pipeline produces artifacts for (currently only x86_64), and the C library is
new enough. There is one artifact for each of the two C libraries:

- The glibc build, `skyline-speeder-<tag>-x86_64.tar.gz`: the daemon is built on Ubuntu 24.04 and needs
  **glibc ≥ 2.38** (`getconf GNU_LIBC_VERSION`) as well as `libelf.so.1` and `libz.so.1`. Debian 13,
  Ubuntu 24.04, Fedora 43, Rocky Linux 10 and newer versions satisfy this.
- The musl build, `skyline-speeder-<tag>-x86_64-musl.tar.gz` (since v0.4.1): built in an Alpine 3.21
  container, it needs **musl ≥ 1.2.5** (the host is musl when `getconf GNU_LIBC_VERSION` reports no glibc and `/lib/ld-musl-*.so.1` exists — a glibc host with the Debian or Fedora musl package installed has that file too, so glibc is asked first; the version is reported by the loader itself), that is, Alpine 3.21 and newer, as well as
  `libelf`, `zlib`, `zstd-libs` and `libgcc`.

The BPF objects in the two artifacts are the same set of files, byte for byte. If the host can run the
artifact, it is installed; otherwise the installer prints one line `warn building from source: <reason>` on
stderr and switches to a source build (for example Debian 12 with a backports kernel: the kernel is new enough,
glibc is not). On a musl host the default path also asks the release API first: when the latest release has no
musl build (v0.4.0 and earlier), it likewise switches to a source build.

| | Prebuilt artifact (default) | Source build (`--source`, or the default path when this host cannot run the artifact) |
|---|---|---|
| Command | `./install.sh`; `--prebuilt` installs only the artifact; `--release <tag>` pins a version | `./install.sh --source` |
| The target host needs | No toolchain at all; the installer installs only runtime dependencies (the package lists in section 2.4) | A build toolchain (clang/LLVM, bpftool, libbpf/libelf/zlib development packages, Rust), which the installer installs and records automatically and `--uninstall` removes again |
| What gets installed | The latest release (or the one `--release` names): changes merged into `main` after it are not included | The source tree being run (for the one-line install, `--ref`, default `main`) |
| Where the BPF objects' types come from | The 6.12 LTS reference header pinned by the release pipeline; at load time CO-RE fixes field offsets against this host's kernel BTF | This host's `/sys/kernel/btf/vmlinux` |
| Integrity check | The `.sha256` next to the artifact must match; a downloaded artifact without one is not installed, and a local file without one is installed unchecked, with a warning | The source tree itself (`scripts/bootstrap.sh` can pin the tarball's digest with `SKYLINE_SHA256`) |

All other steps are identical: the same verifier pass, the same services (systemd units or OpenRC scripts) and
the same configuration template.

### 2.2 Arguments and environment variables

| Argument | Effect |
|---|---|
| (no arguments) | Installs the latest release's artifact when this host can run the released binaries; otherwise prints the reason and switches to a source build (section 2.1) |
| `--source` | Builds from this source tree unconditionally |
| `--prebuilt` | Installs only the published artifact: when this host cannot run it, aborts before the first step with `error --prebuilt/--release cannot install here: <reason>` and does not switch to a build |
| `--release <tag>` | Like `--prebuilt`, pinned to that version (also the way to go back to an older version, section 10.3) |
| `--check` | Only runs the prerequisite checks and changes nothing: reports which path would be taken (`would install the published release` / `would build from source: <reason>`), whether the packages can be installed, the current congestion control and qdiscs (including the NICs the guard would maintain), the relevant settings systemd-sysctl will write at boot, the settings in `/etc/sysctl.conf` that only `sysctl -p`/`sysctl --system` apply, and the `fallback_cc` the configuration would get (a new one, or an existing one whose value is still the installer's own choice). With `SKYLINE_ARTIFACT_URL` set, it reports that it would install that |
| `--no-enable` | Installs or upgrades and starts the daemon, but does not newly attach `skyline_cc`: it does not enable `skyline-speeder-enable.service`, and does not disable it if it was already enabled; if `skyline_cc` was attached with a bare `ssctl enable` before the upgrade, it runs `ssctl enable` once more after the restart (still a manual attach, not attached automatically at boot). At the end it reports the actual attach state from the new daemon's `enabled` and the current default congestion control, and gives the command to attach |
| `--verbose` | Draws no progress bar; the output of every subcommand also goes to the screen; the environment variable `SKYLINE_VERBOSE=1` does the same |
| `--uninstall` | Uninstalls, switches to bbr + fq, and removes the packages the install added (section 11.3) |
| `--restore-pre-install` | Only together with `--uninstall` (on its own it exits with an error): restores the pre-install congestion control and qdiscs instead of bbr + fq (section 11.4) |
| `-h`/`--help` | Prints usage; any other argument exits with `unknown argument` |

Exit code of `--check`: non-zero when the distribution, service manager, kernel, BTF or cgroup requirement is
not met, or when `--prebuilt`/`--release` cannot install on this host; problems such as packages or the
toolchain failing to install only print a `warn` on stderr, and the exit code is still 0. Agents must read
stderr as well.

| Environment variable | Effect |
|---|---|
| `SKYLINE_ARTIFACT_URL` | Skips the release lookup and uses an https URL (a mirror, an internal artifact repository) or a local file path directly as the artifact; takes precedence over `--release`, and when both are given it warns that `--release` was not used; `<URL>.sha256` must be there and match, `<path>.sha256` is checked when it exists (section 2.3). When the install switches to a source build (the host cannot run the artifact, or `--source`), it has no effect, and the installer likewise warns |
| `SKYLINE_VERBOSE=1` | Same as `--verbose` |
| `SKYLINE_FEEDBACK` | After a failure: unset or `ask` asks on the terminal whether to send an error report, `yes` sends one without asking, `no` never asks (section 13.6). `ssctl` reads it too |
| `SKYLINE_REPO` | The repository releases are looked up in, default `CYBERVERSE-Research/skyline-speeder` |

The arguments of `scripts/bootstrap.sh` itself: `--ref <git ref>` (environment variable `SKYLINE_REF`, default
`main`), `--repo <owner/name>` (`SKYLINE_REPO`, the repository the source is taken from), `--src-dir <path>`
(`SKYLINE_SRC_DIR`, default `/usr/local/src/skyline-speeder`); `SKYLINE_SHA256=<digest>` pins the downloaded
source tarball (give it to sudo, not to curl: `curl ... | sudo SKYLINE_SHA256=<digest> bash -s -- --ref <tag>`).
`--repo` only decides where the source comes from; to make the installer take releases from that repository too,
set the environment variable `SKYLINE_REPO`.

### 2.3 Artifact source and checksum

- Default: the latest release on GitHub; `--release <tag>`: the version named;
- `SKYLINE_ARTIFACT_URL`: an https URL or a local file path. For a host that cannot reach GitHub, copy the
  tarball over together with its `.sha256`:

```bash
SKYLINE_ARTIFACT_URL=https://mirror.example/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
SKYLINE_ARTIFACT_URL=/path/to/skyline-speeder-<tag>-x86_64.tar.gz ./install.sh --prebuilt
```

The `<artifact>.sha256` published next to the artifact must match the downloaded (or copied) tarball, or the
install aborts; it aborts too when a downloaded artifact has none, and when a local file has no `.sha256` next to
it, the installer warns that it is unchecked. The `MANIFEST` in the tarball records the tag, the commit, the
reference header's digest, `libc=`, and the SHA-256 of every binary and object: the full content goes into the
install log, and the summary shows one line of provenance (artifact name, commit, whether the sha256 was checked).
An artifact whose `libc=` does not match this host's C library (for example, `SKYLINE_ARTIFACT_URL` pointing at the
wrong file) is rejected before the verifier, with a note on which one to use.

What is installed is a published release, not `main`: changes merged into `main` after the latest release
reach the default install only with the next release.

### 2.4 What the installer does, step by step

| Step (its name in the output) | What it does |
|---|---|
| Checking the system | The distribution and its package manager, the service manager; kernel ≥ 6.12, BTF, cgroup v2 mounted at `/sys/fs/cgroup` (on an OpenRC host where it is not mounted yet, starts the `cgroups` service); whether this host can run the released binaries |
| Installing packages | See the package lists and planning rules below |
| Downloading the release artifact | Prebuilt path: fetches the artifact, checks the sha256 and the `MANIFEST` |
| Preparing the Rust toolchain / Building the BPF objects / Building the Rust control plane | Source-build path: rustup (version pinned by `rust-toolchain.toml`), `make bpf`, `cargo build --release` |
| Running the kernel verifier | Before any file is replaced, runs `--validate-only --verify-bpf` (section 4) with the **new** daemon and the new objects: with the installed configuration, or the template on a first install, and `bpf_dir` pointing at the new objects; the daemon is first placed at `/usr/local/sbin/.skyline-speederd.new` (`/tmp` may be noexec). A failure aborts the install, leaving the installed files and the running daemon untouched |
| Installing files | The prebuilt path copies the files from the artifact directly (the daemon is the very file the verifier ran, renamed into place); the source-build path runs `infra/install-guest.sh`. The file list is in section 2.5 |
| Configuring speeder.toml | Creates the configuration from the template only when it does not exist; rewrites the egress NIC and `fallback_cc` while they still hold what the installer put there (see below). These changes do not affect the conclusion of the verifier step above: `tc_interface` plays no part in validation, and `fallback_cc` is only ever changed to an algorithm this host's kernel has |
| Starting skyline-speederd | On an upgrade it is `Draining live flows (up to 60 s), restarting skyline-speederd` (section 10) |
| Attaching skyline_cc | `systemctl enable --now skyline-speeder-enable.service` (skipped with `--no-enable`) |
| Verifying | Confirms with the new daemon that `enabled` is `true` (and, for versions with the guard, `armed`), and that the default congestion control really is `skyline_cc`; if not, restarts the enable unit once and checks again, and if it still does not hold, fails and points to `ssctl enable` and the enable unit's log |

**Packages.** The prebuilt path installs only runtime dependencies: apt `curl ca-certificates tar iproute2`;
dnf `curl ca-certificates tar
iproute iproute-tc` plus `libelf.so.1` and `libz.so.1` requested by soname; apk `curl ca-certificates tar iproute2
bash libelf zlib zstd-libs libgcc`. A source build installs a different list: apt `build-essential pkg-config clang llvm
libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2`; dnf `gcc make pkgconf-pkg-config clang llvm libbpf-devel
elfutils-libelf-devel zlib-devel bpftool curl ca-certificates tar iproute iproute-tc`; apk `build-base pkgconf clang
llvm libbpf-dev elfutils-dev zlib-dev linux-headers bpftool curl ca-certificates tar iproute2 bash libgcc`.
`iproute2` (`iproute-tc` on Fedora/RHEL) is required at run time: the guard uses its `tc` to maintain the
NIC's root qdisc.

- apt first produces a plan (`apt-get -s install`) and installs for real only once it is confirmed to resolve.
  If the previous `dpkg` run was interrupted (for example a VPS reset in the middle of an upgrade), it first
  completes it with `dpkg --configure -a` / `apt-get -f install --no-remove` — the `--no-remove` is deliberate:
  removing packages the operator installed, to make dependencies consistent, is not a decision the installer
  can make on the operator's behalf. When one package will not fit, it tries that package's other versions from
  newest to oldest according to `apt-cache madison` until the whole plan holds (on Debian 12 with the 6.12
  kernel from `bookworm-backports`, that means swapping only `libelf-dev` to `0.192-4~bpo12+1`, deliberately
  not using `-t bookworm-backports`: that would replace 55 packages). If the plan after a version swap would
  remove any package, it **aborts** and lists those packages; when `apt-get update` fails because some
  repository is unreachable, it only warns and continues.
- dnf and apk solve the whole request together, installing either everything or nothing, and when it cannot be
  installed the installer relays their own error. dnf does not install weak dependencies. On the RHEL family,
  `libbpf-devel` is in the CodeReady Builder repository, which is disabled by default; a source build adds
  `--enablerepo=crb` for that one transaction only (`codeready-builder-for-rhel-N-<arch>-rpms` on RHEL,
  `olN_codeready_builder` on Oracle Linux) and does not change any `.repo` file. apk's `bpftool` is in the
  community repository: when the host has not enabled it, `--repository .../community` for the same mirror and
  the same release is added to that one command only, and `/etc/apk/repositories` is not changed.

**Egress NIC (`runtime.tc_interface`).** Rewritten only while the configuration still holds the template value
`data0`; a configuration the operator has changed is left alone (if the NIC it names does not exist, the
installer only warns). It takes the first **Ethernet** device the default route goes through
(`/sys/class/net/<nic>/type` is 1; VLANs, bonds and bridges count too): first the IPv4 default route, then IPv6
(an IPv6-only host has no IPv4 default route). When the default route leaves only through a layer-3 tunnel such
as WireGuard/WARP, tun, gre or ppp, the template value is kept and the installer warns: `skyline_tc` parses
packets by their Ethernet header, and the daemon refuses to attach it to a non-Ethernet device; on such a host,
set it by hand to the physical NIC that carries the tunnel traffic (section 8.1).

**`fallback_cc`.** Chosen when this run creates the configuration file, and again on every later run (an
upgrade, or a reinstall after `--uninstall`, which keeps the file) for as long as the value is still the
installer's own choice. It is the algorithm new connections use when `skyline_cc` is not there (section 8.1),
and the template value is `cubic`; the installer uses instead the algorithm the boot-time sysctl configuration
selects (finding the effective file by systemd-sysctl's rules, for example `bbr` in the
`/etc/sysctl.d/99-bbr.conf` left by a "one-click BBR" script): that is the algorithm this host runs without
Skyline Speeder, and also an algorithm that is certain to be registered before `skyline-speederd` starts on
every boot — systemd-sysctl (on OpenRC, the sysctl service) writes it before then, and the kernel loads its
module along the way. When the boot-time configuration sets nothing, sets `skyline_cc`, or sets an algorithm
this host has not registered at the moment, the value is kept (`cubic` in a new file).

The installer records its choice in `/etc/skyline-speeder/installer-fallback-cc` (section 2.5). A
`fallback_cc` that differs from the recorded name was set by the operator and is never rewritten; one that
matches follows the boot configuration, so a host that moves from cubic to bbr — before a reinstall, say —
gets `bbr` at the installer's next run. A configuration written by an installer from before this record
existed has none: then only the template's `cubic` counts as the installer's choice, and any other value as
the operator's. The choice (`fallback_cc set to …` / `changed from … to …` / `left at …` /
`left as configured …`) is written to the install log, and also to the screen with `--verbose`; `--check`
reports which value the configuration would get.

**Pre-install record.** Before changing anything, the installer records the current congestion control,
`net.core.default_qdisc`, and the root qdisc of every NIC the guard will maintain, in
`/etc/skyline-speeder/pre-install-state` (section 2.5), for the before/after comparison at the end and for
`--uninstall --restore-pre-install`. This snapshot is written once per installation: an upgrade keeps it, so
a run cannot record `skyline_cc` as the "original value" by mistake. A reinstall after `--uninstall` — nothing
of the earlier installation is left but `/etc/skyline-speeder`, and `skyline_cc` is not the default — replaces
it with what the host runs at that point, so the summary's `was` and `--restore-pre-install` describe the same
state, not the one from before the first install. `--check` says when a run would replace it.

**Final summary.** A before/after comparison of the congestion control and qdiscs (such as
`skyline_cc (was bbr)`, `mq/fq on eth0 (was cake), default fq (was cake)`; one line per NIC for something like
a bond carried by several NICs; what was left untouched comes with the reason, such as
`cake on eth0 (bandwidth 90Mbit: it shapes, so it was left alone)`) and who keeps them in place; the sysctl
files that would still set them to something else (judging which file takes effect at boot by systemd-sysctl's
rules: a file of the same name under `/etc` — including a link to `/dev/null` or an empty file — overrides
those under `/run` and `/usr/lib`, and files are applied in filename order; `/etc/sysctl.conf` takes effect at
boot only when a link in `sysctl.d` refers to it). The installer **does not modify** these files: they take
effect first at boot and are then overridden by `skyline-speederd`. Last come everyday commands, the current
values of a few common parameters and where to tune them, and how many packages an uninstall would remove.

### 2.5 Installed files and state

| Path | Contents |
|---|---|
| `/usr/local/sbin/skyline-speederd`, `/usr/local/bin/ssctl` | The daemon and the command-line tool |
| `/opt/skyline-speeder/bpf/*.bpf.o` | The three BPF objects |
| `/opt/skyline-speeder/infra/*.sh` | `boot-enable.sh`/`boot-disable.sh`, which the enable unit calls, `run-in-skyline-cgroup.sh` from section 7, and `apply-guest-profile.sh`, `collect-guest-metrics.sh`, `snapshot-skyline-events.sh` |
| `/etc/systemd/system/skyline-speederd.service`, `skyline-speeder-enable.service` | The two systemd units; on OpenRC hosts, `/etc/init.d/skyline-speederd` and `/etc/init.d/skyline-speeder-enable` (section 12) |
| `/etc/skyline-speeder/speeder.toml` | The configuration; created from the template only when it does not exist, never overwritten by later installs and upgrades, which change in it only a placeholder `tc_interface` and their own choice of `fallback_cc` (section 2.4) |
| `/etc/skyline-speeder/installer-fallback-cc` | The `fallback_cc` the installer last chose for `speeder.toml`, one name: while the configuration still holds it, every run chooses again from the boot configuration (section 2.4) |
| `/etc/skyline-speeder/pre-install-state` | The pre-install snapshot: `PRE_INSTALL_CC`, `PRE_INSTALL_QDISC`, and `PRE_INSTALL_QDISC_DEV`/`PRE_INSTALL_ROOT_QDISC` (two space-separated lists, of NICs and of root-qdisc summaries, matching one to one; empty when the NIC is not known). Written once per installation: an upgrade keeps it, a reinstall after `--uninstall` replaces it (section 2.4); a snapshot written by 0.2.0 lacks the last two keys, which an upgrade adds |
| `/etc/skyline-speeder/added-packages`, `added-rustup` | The packages and rustup this install added to the host (section 11.3 uninstalls based on them) |
| `/var/log/skyline-speeder-install.log` | The install log |
| `/usr/local/src/skyline-speeder` | The source tree, when installed through the bootstrap script |
| `/run/skyline-speeder/` | Runtime: the control socket `speeder.sock`, the event log `events.jsonl`, `state.json` (section 13) |
| `/var/lib/skyline-speeder/reports/` | The errors skyline-speederd stopped on, waiting for the next `ssctl` on a terminal to offer them as an error report (section 13.6); `StateDirectory=` in the unit (`checkpath` under OpenRC), mode 0700 |
| `/sys/fs/cgroup/skyline-speeder`, `/sys/fs/bpf/skyline-speeder` | The cgroup of section 7; the directory under bpffs |

### 2.6 Releases older than the guard

`--prebuilt`/`--release` installs a published release, which may be older than the `install.sh` running.
Releases v0.2.0 and earlier have no guard (section 9): `ssctl enable` itself does not touch qdiscs and does not
keep the default congestion control in place afterwards; the enable unit shipped with the same release (with
its own `boot-enable.sh`) writes `net.core.default_qdisc=fq` once on every attach and every boot, does not
replace the NIC's root qdisc, and nothing keeps that value in place. The installer warns about this
(`... is a release older than the guard ...`), and the summary's qdisc line says
`(not managed by this release)`; G7 of section 6 does not apply then. Decide by the version the daemon reports
(`sv version` is empty for these versions), not by whether the JSON has a `guard` key: `ssctl` since 0.3.0
prints that key for any daemon.

---

## 3. Manual step-by-step deployment (without `install.sh`)

Use this only when you need step-by-step control; the one-line install does exactly the following. A manually
installed host has no pre-install snapshot and no package record, so `--uninstall` removes no toolchain and
`--restore-pre-install` has nothing to restore from (section 11.3). Run everything as root:
`infra/install-guest.sh` runs `make bpf` and `cargo build` as root, so Rust must be installed for root.

```bash
# 3.1 Toolchain (Debian/Ubuntu)
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config clang llvm \
    libbpf-dev libelf-dev zlib1g-dev bpftool curl iproute2   # the guard uses tc to maintain the NIC qdisc
# Fedora / RHEL family (on the RHEL family libbpf-devel is in CodeReady Builder: add --enablerepo=crb):
#   dnf install -y gcc make pkgconf-pkg-config clang llvm libbpf-devel elfutils-libelf-devel zlib-devel bpftool curl iproute iproute-tc
# Alpine (bpftool is in the community repository):
#   apk add build-base pkgconf clang llvm libbpf-dev elfutils-dev zlib-dev linux-headers bpftool curl iproute2 bash libgcc

# Debian 12 + the 6.12 kernel from bookworm-backports: linux-headers-* pulls libelf1 up to 0.192, while bookworm's
# libelf-dev depends on libelf1 (= 0.188-2.1), so the apt-get above fails with "held broken packages". Swap only
# this one package, taking the version from the same source as the library on the host:
#   apt-cache madison libelf-dev
#   apt-get install -y libelf-dev=0.192-4~bpo12+1
# Do not use -t bookworm-backports: that switches curl, iproute2, bpftool and libbpf1 to the other source as well (55 packages, measured).

# 3.2 Rust (rust-toolchain.toml pins the version; rustup follows it automatically)
command -v cargo >/dev/null || {
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
}

# 3.3 Build and install (without --confirm-install it only prints what it would do, exit code 2)
./infra/install-guest.sh --confirm-install
```

`make bpf` generates `vmlinux.h` from this host's `/sys/kernel/btf/vmlinux` by default. The objects are CO-RE:
the header only has to define the types the code uses, and field offsets are fixed against the running kernel
at load time — the release artifacts are exactly that, compiled against the 6.12 reference header and loaded on
newer kernels. To build for another machine, point it at that machine's kernel BTF, or hand it a ready-made
header (that path needs neither bpftool nor this host's BTF):

```bash
make VMLINUX_BTF=/path/to/target/vmlinux bpf
make PREBUILT_VMLINUX_H=/path/to/vmlinux.h bpf
```

CO-RE can fix offsets, but not a field that was renamed or removed: when objects move to another kernel, you
**must** first run the verifier of section 4 on the target host. `bpf/include/vmlinux.h` is a build artifact
and is excluded in `.gitignore`.

`install-guest.sh` installs the binaries, objects, scripts and the two units (two init scripts on OpenRC hosts)
from the table in section 2.5, creates `/etc/skyline-speeder/speeder.toml` from `config/speeder-guest.toml`
only when it does not exist, and enables `skyline-speederd.service` at boot; it **deliberately does not
enable** `skyline-speeder-enable.service` — attaching changes the congestion control of every new connection
on the host, which is an operator decision, made explicitly by step ③ of section 5.1.

```bash
# 3.4 Egress NIC: the template value data0 is the test bed's interface name and will almost never match a real host.
# Take the word after "dev" (in a route without via, such as `default dev wg0 scope link`, it is not in column 5);
# IPv4 first, then IPv6; Ethernet devices only (type 1; VLAN/bond/bridge are too), skyline_tc is not attached to tunnels
DEV=$({ ip -o route show default; ip -6 -o route show default; } 2>/dev/null \
  | awk '$1 == "default" { for (i = 2; i < NF; i++) if ($i == "dev" && $(i + 1) != "lo") print $(i + 1) }' \
  | while read -r d; do
        if [ "$(cat "/sys/class/net/$d/type" 2>/dev/null)" = 1 ]; then echo "$d"; break; fi
    done)
test -n "$DEV"    # fails: no default route, or the default route goes only through a tunnel -- see the note below
sed -i "s|^tc_interface = \".*\"|tc_interface = \"$DEV\"|" /etc/skyline-speeder/speeder.toml
grep '^tc_interface' /etc/skyline-speeder/speeder.toml

# 3.5 fallback_cc: the default algorithm before skyline_cc is attached, i.e. the host's own algorithm (section 8.1)
CC=$(sysctl -n net.ipv4.tcp_congestion_control)
[ "$CC" = skyline_cc ] || sed -i "s|^fallback_cc = \".*\"|fallback_cc = \"$CC\"|" /etc/skyline-speeder/speeder.toml
grep '^fallback_cc' /etc/skyline-speeder/speeder.toml
```

If `test -n "$DEV"` fails although a default route exists, the route leaves only through a layer-3 tunnel such
as WireGuard/WARP, tun, gre or ppp: set `tc_interface` by hand to the physical NIC that carries the tunnel
traffic (`install.sh` likewise keeps the template value and warns in this case). When `tc_interface` is a VLAN,
bond or bridge, fill it in as is; the guard maintains the physical NICs under it (section 9).

Then run section 4 (verification), section 5 (start and attach) and section 6 (gates) in order. For how to
upgrade a manually installed host, see section 10.2.

---

## 4. Verification before start

```bash
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf
```

It reads the configuration on disk, probes the kernel's capabilities, runs all three BPF objects through the
kernel verifier, and exits: **it attaches no program and writes no sysctl**; all it creates is
`/run/skyline-speeder/` and an empty `events.jsonl`. stdout is the daemon's status JSON (including
`capabilities`); when everything passes, the last line on stderr is `all BPF objects passed the kernel verifier`
and the exit code is 0. When a capability is missing it prints the status JSON first and then exits non-zero,
so the reason can be read on failure too. Without `--verify-bpf` it only validates the configuration and the
capabilities.

Check the six hard capabilities one by one:

```sh
out=$(skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only --verify-bpf); rc=$?
for k in btf bpffs cgroup_v2 fq_available struct_ops fallback_cc_available; do
    printf '%s\n' "$out" | awk -v k="\"$k\":" '!f && $1 == k { v = $2; gsub(/[",]/, "", v); print v; f = 1 }' \
        | grep -qx true || echo "capability $k is false"
done
[ "$rc" -eq 0 ] && echo "verifier OK"
```

- `rack_reo_hook: false` is **normal**: it only means early-loss stays in observation mode, and does not block
  deployment. That is why `capabilities.notes` always has an `early-loss remains observation-only ...` entry on
  a stock kernel, and content in the ATTENTION section of `ssctl status` does not mean the deployment failed.
- `fallback_cc_available: false` (`Error: configured fallback congestion control is unavailable`):
  this kernel neither has the algorithm named in `fallback_cc` registered nor a loadable module `tcp_<name>`
  for it; `notes` lists the algorithms registered on this host. Change `fallback_cc` to one of them (usually
  `bbr`, which the host was using); see sections 8.1 and 13.4.

---

## 5. Start and attach

### 5.1 Start sequence (the order cannot be reversed)

```bash
# ① Start the resident process
systemctl enable --now skyline-speederd.service

# ② Wait for the daemon to answer — you must wait; do not assume it is available at once
timeout 60 sh -c 'until ssctl status --json >/dev/null 2>&1; do sleep 1; done'

# ③ Attach skyline_cc and switch the default algorithm (the attach at boot takes this same path)
systemctl enable --now skyline-speeder-enable.service
```

`skyline-speederd.service` has no readiness notification: in the first few seconds after the process comes
up, the socket may not exist yet, and `ssctl` run then reports
`connect to /run/skyline-speeder/speeder.sock (is skyline-speederd running?)`, with
`No such file or directory` on the next, `Caused by` line. This is only a start-up race, which is why step ②
waits — for the daemon to answer, not for the socket file to appear: a daemon that was killed leaves its socket
file behind, and OpenRC, unlike systemd's `RuntimeDirectory=`, does not empty `/run/skyline-speeder` on stop.

> [!IMPORTANT]
> **`skyline-speederd.service` does not attach `skyline_cc` automatically after it starts.** It only keeps the
> process resident, and writes no sysctl apart from the tier-1 sysctls declared in `[rack_tuning]` (the shipped
> template declares none);
> without step ③, new connections keep using the host's current default algorithm, **with no error of any
> kind**. `skyline-speeder-enable.service` exists precisely to remove this silent failure: its `boot-enable.sh`
> waits for the daemon to answer (up to 60 seconds) and then runs `ssctl enable`, attaching once on every boot;
> stopping it makes `boot-disable.sh` run a drain (section 11).

### 5.2 What `ssctl enable` does

1. Validates the configuration and the six capabilities (section 1.3);
2. On the first attach, first writes `fallback_cc` as the default congestion control (a baseline: if the attach
   fails, the host stays on an algorithm known to work), then loads and registers `skyline_cc` (on kernels
   ≥ 7.1 the struct_ops map that registers it is called `skyline_cc_txs`; the algorithm name is still
   `skyline_cc`), and makes processes in the cgroup of section 7 switch to it when they connect;
3. Only after a successful attach writes `skyline_cc` into `net.ipv4.tcp_congestion_control` — the kernel does
   not accept an algorithm name that is not registered yet;
4. Arms the guard and immediately runs one full check (section 9): with `[guard] qdisc = true` (the default),
   writes `fq` into `net.core.default_qdisc` and replaces the egress NIC's root qdisc with `fq` (on a
   multi-queue NIC, an `mq` whose child queues are all `fq`; for a VLAN, bond or bridge, the physical NICs under
   it are changed instead);
5. Turns on first-flight redundancy: every SYN-ACK this host sends that has a socket (whatever congestion
   control its connection will run: none has been chosen when it is sent), and the SYN and the first 64 KiB
   (retransmissions within them included) of `skyline_cc` connections, are each sent twice, the second copy
   10 ms later (section 8.1).

`ssctl enable` without `--modules` keeps the daemon's current module set: after `ssctl enable --all-off`, it
takes `--modules early-loss,adaptive-cwnd,loss-classifier,pacing` to get back to all on (or a daemon restart).

> [!IMPORTANT]
> **`ssctl enable` takes effect host-wide.** After the attach, **every** new TCP connection on this host (not
> just those made by processes in `/sys/fs/cgroup/skyline-speeder`) uses `skyline_cc` by default, and the guard
> keeps it that way: change the sysctl back by hand and it is changed back again within `[guard] interval_s`
> seconds (5 by default), and the log even records it as "something else on this host changed it".
> If you really want only processes in the cgroup to use `skyline_cc`: set `[guard] interval_s = 0` (no
> periodic check; takes effect when the daemon restarts) and, after every `enable`, write `fallback_cc` back
> into `net.ipv4.tcp_congestion_control` by hand — processes in the cgroup are still switched to `skyline_cc`
> by `skyline_policy` when they connect. This holds only until the next `enable`, **including the one the
> enable unit runs at boot**. Falling back is symmetric: `ssctl drain` first writes `fallback_cc` back into the
> sysctl, then waits for existing connections (section 11.1).

---

## 6. Verification gates

```sh
# G1 the daemon and the attach unit have both started
systemctl is-active skyline-speederd.service skyline-speeder-enable.service

# G2 (optional) struct_ops is registered: needs bpftool, which hosts with a prebuilt install do not have; G3 is authoritative.
#    On kernels >= 7.1 the map is called skyline_cc_txs; the grep below matches it too
bpftool struct_ops show | grep -q skyline_cc

# G3 the algorithm is in the kernel's available list
sysctl -n net.ipv4.tcp_available_congestion_control | grep -qw skyline_cc

# G4 it is the system default
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = skyline_cc ]

# G5 the daemon itself says it is attached (sv is defined at the top of this document)
[ "$(sv enabled)" = true ]

# G6 starts at boot
systemctl is-enabled skyline-speederd.service skyline-speeder-enable.service

# G7 the guard is armed and the qdiscs are in place (skip the last two with [guard] qdisc = false; skip the last one when there is no NIC to maintain).
#    The last one looks at the NICs the guard actually maintains (guard.live.devices: tc_interface itself,
#    or the physical NICs under it when it is a VLAN/bond/bridge); the root of each must be fq or mq/fq
[ "$(sv armed)" = true ]
[ "$(sysctl -n net.core.default_qdisc)" = fq ]
#    (a NIC whose root qdisc cannot be read -- for example one that is down -- is null in the JSON and also counts as a failure)
q=$(ssctl status --json | grep -E '^ +"qdisc": ("|null)' | sed -E 's/.*"qdisc": "?([^",]*)"?,?$/\1/')
[ -n "$q" ] && ! printf '%s\n' "$q" | grep -vqxE 'fq|mq/fq' && echo "qdisc OK:" $q
```

The verdict is at the right of the `ssctl status` report's title: `● ACCELERATING` (attached and the host-wide
default), `○ ATTACHED, NOT DEFAULT` (attached, but new connections bypass it, which amounts to no effect),
`○ DRAINING` (a drain was run; new connections are back on `fallback_cc` and skyline_cc only serves the
remaining connections, section 11.1), `○ STANDBY` (not attached). On `ATTACHED, NOT DEFAULT` or `STANDBY`, the
ATTENTION section at the bottom lists each problem and what to do about it. Every `ssctl` subcommand prints a
human-readable report by default; scripts that parse output always add `--json`. When the reply's `ok` is
`false`, `ssctl` exits with 1.

When the last two checks of G7 fail, look at the DRIFT GUARD section of `ssctl status` (in `--json`, `notes`
and `last_error` of `guard`; print the whole object with
`ssctl status --json | sed -n '/^    "guard": {/,/^    }/p'`): when the root qdisc is a deliberately built
shaping setup such as `htb`/`tbf`/`netem`, or a `cake` with a bandwidth set, the guard leaves it alone by
design; when `tc_interface` is a tunnel whose root is `noqueue` and no physical NIC can be found under it,
there is no NIC to maintain — neither case is a failed deployment (section 9).

**G8 Survives a reboot** (strongly recommended): `systemctl reboot`, then run G1–G7 again after the reboot.

**G9 Live-traffic check**: confirm that real connections actually use this algorithm, not just that the sysctl
value is right.

```bash
ss -tin | grep -c skyline_cc          # should be > 0 (established old connections do not switch midway; wait for new ones)
ssctl flows                           # CONNECTIONS section: the coverage line and one row per connection
```

`ssctl flows` is that `ss` run for you by the daemon: the coverage line says "how many TCP connections on the
host run on `skyline_cc`", and below it each connection is listed with RTT, cwnd, pacing, delivery rate and
retransmission share (sorted by bytes sent, at most 50; the rest are shown only as a one-line count,
"N more, not shown"), followed by the parameters in force and the algorithm's decision counters.

---

## 7. The cgroup prerequisite (M1 dynamic RTO)

`skyline_policy` is attached to the cgroup `/sys/fs/cgroup/skyline-speeder`: **only connections made by
processes in this cgroup** pass through this BPF code, and no error of any kind is reported when a process is
not moved in. Only M1 tier-2 dynamic RTO tuning is affected; `skyline_cc` (a global struct_ops) and
`skyline_tc` (interface-level TC) are **not** subject to this restriction.

**Dynamic RTO is off by default.** The daemon clears it on every start, and `[rack_rto]` in the configuration
file is **not** pushed at start (the shipped template has no such section, i.e. it is off). To turn it on:

```bash
ssctl set-rack-rto --srtt-permille 1100 --floor-us 20000 --ceiling-us 200000 --warmup-samples 4   # push the values given on the command line
ssctl reset-rack-rto      # or: push the values of [rack_rto] in the configuration file
ssctl set-rack-rto --disable   # turn it off unconditionally
```

Both are absolute overwrites and must be run again after every daemon restart. For what the parameters mean,
the RTO ceiling (`--rto-max-*`, needs kernel ≥ 6.15) and recommended values, see `docs/usage.md` section 5.

Moving processes in:

```bash
/opt/skyline-speeder/infra/run-in-skyline-cgroup.sh <service start command...>
```

Root is needed only to write `cgroup.procs`; when it is called through `sudo`, the wrapped command runs as
`$SUDO_USER`. Move a systemd service with a drop-in (the `+` prefix raises privileges; writing `cgroup.procs`
needs root):

```ini
# /etc/systemd/system/<unit>.d/10-skyline-cgroup.conf
[Unit]
After=skyline-speederd.service skyline-speeder-enable.service

[Service]
ExecStartPost=+/bin/sh -c 'echo $MAINPID > /sys/fs/cgroup/skyline-speeder/cgroup.procs'
```

Diagnosis (the DYNAMIC RTO section of `ssctl flows`; when dynamic RTO is off, the CGROUP POLICY section, which
shows only *connections seen*; all counters are in `rack_rto.stats` of `--json`):

| Symptom | Conclusion |
|---|---|
| `established_cb` (*connections seen*) is 0 | No process's connections pass through `skyline_policy` → the target process is not in the cgroup. This line is shown whether or not RTO tuning is on (0.5.1 and earlier showed it only once it was on); with RTO tuning on, the ATTENTION section says so as well |
| `established_cb` grows, `subscribe_ok` and `rtt_callbacks` are 0 | RTO tuning is not on (it is off by default after the daemon starts) → run `set-rack-rto` / `reset-rack-rto` above |
| `subscribe_err` is non-zero | Subscribing to the RTT callback failed → check the kernel's sockops capability |
| `subscribe_ok` non-zero, `applied` 0, `rejected` non-zero | The kernel rejects the values pushed → they are outside the kernel's valid range for RTO |
| `rto_max_rejected` grows, `rto_max_applied` is 0 | Kernel < 6.15, which has no `TCP_RTO_MAX_MS`; the RTO ceiling has no effect, the floor works as usual |

---

## 8. Configuration and tuning

### 8.1 Configuration file essentials

The configuration is `/etc/skyline-speeder/speeder.toml`; its template is `config/speeder-guest.toml` in the
repository (called `config/speeder.toml` in the release artifact). For each field's type, range and when it
takes effect, see `docs/02-interface-reference.md` section 6.

- All four modules (`early-loss`/`adaptive-cwnd`/`loss-classifier`/`pacing`) are enabled by default;
- `[redundancy]` is on by default: `first_kib = 64`, `delay_ms = 10` (section 5.2);
- `[rack_tuning]` is left empty as a whole: `skyline-speederd` does not take over the defaults of the global
  sysctls;
- `[rack_rto]` and `[retransmit_dscp]` are off by default, and even when written into the file they are **not
  pushed when the daemon starts**: that takes `ssctl reset-rack-rto` / `reset-retransmit-dscp` (pushing the
  values in the file) or `set-*` (pushing values from the command line).
  Since 0.5.1 the status truthfully shows them as off (in `--json`, `config.enabled` is `false` and the file's
  values are in `declared`), and when the file turns them on but they have not been pushed, the ATTENTION
  section of `ssctl status` points this out; in 0.5.0 and earlier, `config` shows them as on, following the
  file. `dscp_value` is a placeholder: before enabling it, the network side must supply the actual code point
  (`docs/usage.md` section 6);
- `[guard]` defaults to `interval_s = 5`, `qdisc = true` (section 9);
- `fallback_cc`: the algorithm new connections use when `skyline_cc` is not there. It is written before every
  enable attaches, by `ssctl drain`, and by the enable unit's ExecStop; not when the daemon itself stops. It can
  name an algorithm the kernel has registered (`/proc/sys/net/ipv4/tcp_available_congestion_control`), or one
  whose module `tcp_<name>` the kernel can load automatically (since 0.4.3; earlier versions accept only
  registered ones);
- `runtime.tc_interface` must be the Ethernet device that carries the traffic: `skyline_tc`'s retransmit DSCP
  marking and first-flight redundancy parse packets by their Ethernet header, and the daemon will not attach it
  to a layer-3 tunnel such as WireGuard/WARP, tun, gre or ppp (the reason is given in the ATTENTION section,
  `capabilities.notes` in `--json`; `set-retransmit-dscp` then fails). The root qdisc the guard maintains is
  also this NIC's.

> [!CAUTION]
> Since 0.5.0 the daemon rejects keys it does not know: a key in the wrong section (for example
> `loss_inflation_max_ratio` written into `[adaptive_cwnd]`) or a misspelled key makes both `--validate-only`
> and daemon start-up fail, with an error that lists each such key and, when a key belongs to another section,
> names that section. **Daemons 0.4.x and earlier silently ignore them**, and `--validate-only` passes as well.
> Whatever the version, after a change check the values in force in the PARAMETERS IN FORCE section of
> `ssctl flows`.

### 8.2 Live tuning

```bash
ssctl set-module-config --cruise-pacing-gain 1.3     # M2/M3/M4 coefficients and limits
ssctl reset-module-config                            # back to the values in the configuration file
```

- `set-module-config` is an **absolute overwrite**: every call sends all 17 coefficients, and those not given
  on the command line take `ssctl`'s built-in defaults, not the values in the configuration file (the two are
  the same on a fresh install; they differ on a host upgraded from 0.3.x or earlier that kept its old
  configuration). The same goes for `--disable-prr-pacing` and `--disable-auto-pacing`: leaving them out means
  on. If any one parameter is out of bounds, the whole command is rejected and the configuration in force stays
  unchanged.
- New coefficients are written into the configuration slot not currently pointed to, and then the
  configuration sequence number is pointed at it; each connection copies it into its own state at an RTT
  boundary and from then on reads only its own copy, so what it reads within one round is never a mix of old
  and new, even when a script pushes twice in a row (0.4.x, when pushed twice within one RTT, rewrote a slot
  that connections were still reading). Coefficients changed while `skyline_cc` is not attached take effect at
  the next `enable`.
- `set-rack-rto` is also an absolute overwrite, but it rewrites the single set of RTO parameters directly,
  without going through the configuration slots (section 7).
- For the four ready-made preset recipes (conservative, default, aggressive, high random loss) and the
  consequences of raising or lowering each parameter, see `docs/usage.md` section 4.

### 8.3 Persistent changes

Live changes live only in the daemon's memory and are lost on restart. To make them last:

```bash
cp /etc/skyline-speeder/speeder.toml /etc/skyline-speeder/speeder.toml.bak
vi /etc/skyline-speeder/speeder.toml
skyline-speederd --config /etc/skyline-speeder/speeder.toml --validate-only    # if this does not pass, do not restart
systemctl restart skyline-speederd.service
```

Restarting the daemon also restarts the enable unit when it is active (`Requires=`): its ExecStop first drains
(up to 60 seconds), and it attaches again once the new daemon is up. Afterwards, check PARAMETERS IN FORCE as
section 8.1 advises.

---

## 9. The guard: keeping the congestion control and qdiscs in place

`default_qdisc` only affects qdiscs created afterwards, and a NIC's root qdisc has usually been created before
it takes effect, so changing the sysctl alone does not reach the NIC; and the "one-click BBR" scripts common on
VPSes write `bbr` and `cake`/`fq_pie` into `/etc/sysctl.d`, after which any `sysctl --system` quietly flips the
congestion control back to `bbr`, and from then on new connections bypass `skyline_cc` without any error. So
from `enable` to the next `drain`, `skyline-speederd` exclusively owns three settings:
`net.ipv4.tcp_congestion_control` and, with `[guard] qdisc = true` (the default), `net.core.default_qdisc` and
the root qdisc of the managed NICs. It checks once immediately on `enable` and then again every
`[guard] interval_s` seconds (5 by default); whatever has been changed is changed back, with a log line
`guard: <item> <old value> -> <new value> (...)`. Neither the guard nor the installer **modifies**
`/etc/sysctl.conf` or any file under `/etc/sysctl.d`. `drain` lifts the guard immediately and touches no qdisc.
The full rules are in `docs/02-interface-reference.md` section 9.

**Managed NICs.** That is `runtime.tc_interface` itself; when its root is the kernel default `noqueue` (VLAN,
bond, bridge), packets actually queue in the root qdisc of the physical NICs under it, so the guard walks down
`/sys/class/net/<nic>/lower_*` (at most 4 levels, each device looked at once) and manages the physical/virtio
NICs that have a `device` link (a bond's slave NICs, a VLAN's real NIC, a bridge's physical ports), with the
path written into the correction record, such as `eth0 (under bond0) root qdisc cake -> fq`. Ports without a
`device` link (a virtual machine's tap, a container's veth, ifb) are never touched.
When the root is `noqueue` and no NIC can be found under it (a kernel WireGuard device, a bridge with only taps
attached), it only records
`<nic> is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard; no qdisc checked`.
tun and PPP devices (OpenVPN, wireguard-go, the WARP client, PPPoE) have their own default qdisc; when one is
used as `tc_interface`, the guard manages its own root. The managed NICs are listed in the DRIFT GUARD section
of `ssctl status` (`guard.live.devices` in `--json`).

**Only qdiscs with no configuration worth keeping are replaced** (`pfifo_fast`, `fq_codel`, an unshaped
`cake`, `fq_pie` and the like). Deliberately built shaping, classful and offload qdiscs (`htb`, `tbf`,
`netem`, `mqprio` and the like), a `cake` with a bandwidth or `autorate-ingress` set, a `noqueue` the operator
set on a physical NIC by hand, and unknown kinds are all left alone, with only an explanation in `notes`, for
example `eth0 root qdisc cake (bandwidth 90Mbit) looks deliberate; left alone`.

Commands to check:

```bash
ssctl status                                                          # DRIFT GUARD section
ssctl status --json | sed -n '/^    "guard": {/,/^    }/p'            # the whole guard JSON object
journalctl -u skyline-speederd.service | grep 'guard:'
```

| Symptom | Conclusion and action |
|---|---|
| `cc_restored` keeps growing, and the log repeatedly shows `guard: tcp_congestion_control ... (something else on this host changed it)` | Something on the host keeps writing this sysctl (commonly `sysctl --system`/`sysctl -p` re-applying the `bbr` in `/etc/sysctl.conf` or `/etc/sysctl.d/*.conf`). The guard keeps changing it back, so the effect is unharmed; to get rid of it for good, find and delete that line (the install summary lists these files) |
| `notes` contains `looks deliberate; left alone` or `is not a kind the guard knows is safe to replace` | The NIC's root qdisc is a shaping, classful or offload kind, or a `cake` with a bandwidth set, or an unknown kind: left alone by design, so as not to destroy a shaping setup |
| `notes` contains `is a virtual device (noqueue is its kernel default) and no NIC under it is visible to the guard` | `tc_interface` is a device whose root is `noqueue` with no physical NIC under it, and the guard checks no root qdisc. Change `runtime.tc_interface` to the physical NIC that carries the traffic and restart `skyline-speederd` |
| `capabilities.notes` contains `is not an Ethernet device` | `tc_interface` is not an Ethernet device, so `skyline_tc` is not attached (`set-retransmit-dscp` then fails). Same action as above |
| `last_error`/`notes` contains `tc is not installed` | Install `iproute2` (`iproute-tc` on Fedora/RHEL), then `ssctl enable` |
| `notes` contains `the last replace failed; retrying in <N> s` | A replacement did not take effect, and the periodic check retries with backoff (starting at 30 seconds, or at `interval_s` when that is longer, doubling with every failure, up to 300 seconds); see `last_error` for the reason. Once the cause is fixed, run `ssctl enable` to retry immediately. With `interval_s = 0` it reads `retried on the next ssctl enable` |
| `last_error` contains `left alone: default_qdisc is` | `default_qdisc` is not `fq` (the write was refused, or something changed it again right away); a newly created `mq` on a multi-queue NIC builds its child queues from it, so the guard does not touch the root qdisc. First find out who is changing `default_qdisc` |
| `last_error` contains `did not finish within 10000ms and was killed`, or `a previous tc has not exited yet (rtnl lock held?); skipped` | `tc` is stuck on the kernel's rtnl lock: until the killed `tc` exits, every `tc` the guard would run is skipped at once, so they do not pile up; after the lock is released, the next check recovers by itself. If it persists, find out who is holding rtnl for so long (for example a stuck NIC driver) |
| `enabled` is `true` but `armed` is `false` | The normal state after a drain timed out (section 11.1): the struct_ops is still attached, the guard is disarmed. Run `ssctl enable` to resume acceleration |

**Opting out.** To have it manage only the congestion control: `[guard] qdisc = false`; for no periodic check:
`interval_s = 0` (`enable` still runs one). Both take effect only after changing the configuration file and
restarting `skyline-speederd`.

---

## 10. Upgrades

### 10.1 Hosts installed with the one-line install

Just run the installer again, with `--source`/`--no-enable` again if they were used originally (rerunning
without `--source` on a host built from source switches it to the published artifact). On a host installed
through the bootstrap script, rerun the same one-line command. What the installer does:

1. The new daemon and objects first pass the kernel verifier (with the installed configuration and `bpf_dir`
   pointing at the new objects); only then are the installed binaries, objects, scripts and units replaced, and
   only then is the running daemon touched; if verification fails, nothing is replaced (section 10.3);
2. When `skyline-speederd.service` is active, it runs `systemctl restart skyline-speederd.service`. When the
   enable unit is active, it restarts along with it: its ExecStop (`boot-disable.sh`) runs `ssctl drain` —
   first disarming the guard and writing `fallback_cc` back, then waiting up to 60 seconds for existing
   connections (over SSH it always runs into the timeout, harmlessly) — and it attaches again once the new
   daemon is up;
3. A host where `skyline_cc` was attached with a bare `ssctl enable` (the enable unit is not active, and the
   current default congestion control is `skyline_cc`) has no ExecStop to drain for it, and the daemon
   deliberately writes no sysctl when it stops, so the installer runs `timeout 75 ssctl drain --timeout 60`
   itself before the restart (a failure is only logged). After the restart, without `--no-enable` the enable
   unit attaches (and from then on at every boot); with `--no-enable`, it runs `ssctl enable` once more (still a
   manual attach). It looks only at the current default, not at the old daemon's `enabled`, because a drain
   that timed out has already written `fallback_cc` back but leaves `enabled: true` — `skyline_cc` was taken
   off that host on purpose.

After the upgrade:

- An existing `/etc/skyline-speeder/speeder.toml` is not overwritten; new configuration sections it lacks take
  effect with their defaults. When upgrading to 0.5.0 or later, if the file has keys the new daemon does not
  know (in the wrong section, misspelled), the installer stops at the verifier step with an error listing those
  keys, leaving the installed files and the old daemon untouched (section 10.3): fix the file as the error says
  and rerun;
- Live changes made with `ssctl` exist only in the old daemon's memory and do not survive the restart; the
  installer has written the old `ssctl status --json` into the install log, so push them again from it if
  needed;
- The summary shows `upgraded <old> -> <new>`, or `downgraded <old> -> <new>` when going back to an older
  version (binaries v0.2.0 and earlier have no `--version` and show
  `from a release without --version (v0.2.0 or older)`); confirm with the criteria of section 0.2 that the new
  version is running.

### 10.2 Manually installed hosts

`install-guest.sh` only replaces files and **does not restart** the running daemon; the old version stays in
memory. After installing, first run the verifier of section 4 (it does not affect the running daemon); once it
passes:

```bash
# The enable unit is active (a host attached as in section 5): restart restarts it too, and its ExecStop drains by itself, no manual step needed.
# Drain by hand only when it is not active (skyline_cc attached with a bare ssctl enable): stopping the daemon alone does not write fallback_cc back.
# A drain run over SSH always times out and returns non-zero (section 11.1); this is expected.
systemctl is-active -q skyline-speeder-enable.service || ssctl drain --timeout 60 || true
systemctl restart skyline-speederd.service
```

If the enable unit was not active before but this host should have `skyline_cc` attached, start it after the
restart as in step ③ of section 5.1 (or run `ssctl enable` once more, but then it is not attached
automatically at boot).

### 10.3 When the verifier fails, or to go back to an older version

The installer runs the verifier **before** replacing any file: when the verifier fails, it stops, leaving the
installed files and the running daemon untouched. (Installers before 2026-10 replaced the files first and ran
the verifier afterwards: when verification failed, the new files were already on disk, and at the next enable
after a drain the old daemon would read the new `skyline_cc.bpf.o`, which does not pair with it. For a host
left in that state, just rerun the current installer.) First find and remove the cause from the output of
section 4, then rerun; to go back to an older version, install it with `--release <old tag>`, and the summary
says `downgraded` — the configuration file is kept as is. Daemons 0.4.x and earlier ignore new keys they do not know; 0.5.0 and later reject them, so first delete the keys the error lists.

---

## 11. Removal, stopping and uninstallation

### 11.1 Graceful removal: `ssctl drain`

```bash
ssctl drain --timeout 60
```

In order: disarm the guard, and while still holding its lock write `fallback_cc` back as the default congestion
control (after this, no check writes `skyline_cc` back) →
processes in the cgroup are no longer switched to `skyline_cc` → stop first-flight redundancy →
wait for existing `skyline_cc` connections to end naturally (300 seconds by default, overridden by
`--timeout`) → once they have all ended, unregister the struct_ops. From then on new connections fall back to
`fallback_cc`. `drain` does not touch qdiscs: `default_qdisc` and the NIC's root qdisc stay `fq`.

> [!IMPORTANT]
> **A `drain` run over SSH always runs into the timeout**: your own SSH connection is a `skyline_cc` flow and
> will not end while it waits.
> On the timeout, `ssctl` prints `drain timeout expired; struct_ops remains attached, but the default congestion control is
> already back on <fallback_cc> ...` and exits with 1 — recognize it by this message; it is expected. By then
> the sysctl has long been written back and the guard disarmed, so "new connections no longer use
> `skyline_cc`" already holds; `skyline_cc` itself stays registered (`sv enabled` is still `true`, `sv armed`
> is `false`, `sv draining` is `true`) until some later drain sees the connections reach zero, or the daemon
> stops. `ssctl status` and `ssctl flows` then show `DRAINING` and the number of connections left (0.5.1 and
> earlier showed `ATTACHED, NOT DEFAULT` / `BYPASSED` and suggested enabling again, which was a false alarm).
> To resume acceleration, run `ssctl enable`; to make the drain really wait for zero and unregister, run it
> from a serial console or a channel that does not go through this host's `skyline_cc`.

The daemon handles one request at a time: while `drain` waits, every other `ssctl` call queues behind it, so do
not poll the status during a drain.

M1 dynamic RTO and retransmit DSCP marking are **not affected by drain**; turn them off separately:

```bash
ssctl set-rack-rto --disable      # turn RTO tuning off unconditionally (reset-rack-rto goes back to the configuration file's setting)
ssctl set-retransmit-dscp --disable   # turn marking off unconditionally (since 0.5.2; reset-retransmit-dscp goes back to the configuration file's setting)
```

### 11.2 Stopping

```bash
systemctl stop skyline-speeder-enable.service skyline-speederd.service
```

Stopping the daemon (equivalent to SIGTERM) first stops the guard thread, then unregisters all struct_ops and
BPF links; by itself it **writes no sysctl** (deliberately). When the enable unit is active, stopping the
daemon stops that unit first, and its ExecStop (`boot-disable.sh`) writes `fallback_cc` back: it runs
`ssctl drain` first, and only afterwards, if the default is still `skyline_cc` (the daemon is already gone and
the drain could not run), writes it once itself — in the reverse order, the still-armed guard would change the
value just written back into `skyline_cc` again. On a host attached with a bare `ssctl enable`, run
`ssctl drain` before stopping.

`systemctl stop` does not change the boot settings; it is attached again at the next boot. To remove it
persistently: `systemctl disable --now skyline-speeder-enable.service` (the daemon stays resident as usual, but
nothing is attached at boot).

### 11.3 Uninstalling: `install.sh --uninstall`

```bash
./install.sh --uninstall                         # in the source tree; on a host installed through the bootstrap script:
/usr/local/src/skyline-speeder/install.sh --uninstall
```

It first drains (disarming the guard as well; when the enable unit is active, the unit's ExecStop drains as it
stops, without a second wait), disables and deletes the two units (or the two OpenRC scripts), the binaries,
`/opt/skyline-speeder` and `/var/lib/skyline-speeder` (the error reports waiting there, section 13.6),
**keeps** `/etc/skyline-speeder` (so the configuration is still there on a reinstall), the install log, the
bootstrap script's source tree and `/sys/fs/{cgroup,bpf}/skyline-speeder`, and then:

1. **Switches to bbr + fq** (the default): `net.ipv4.tcp_congestion_control=bbr`, `net.core.default_qdisc=fq`,
   and makes sure the root qdisc of the NICs `runtime.tc_interface` resolves to (exactly those the guard
   maintains while armed, regardless of whether `[guard] qdisc` was ever turned off; when it does not exist on
   this host — for example while it is still the template value `data0` — no NIC is picked in its place, and only the NICs recorded in the snapshot are used) is `fq` (on a multi-queue NIC, an `mq` whose child queues are all `fq`). Those already on `fq` are left alone; anything
   that looks deliberately built, such as `htb`/`tbf`/`netem`/a `cake` with a bandwidth set, is left as it is,
   with only a warning and the commands to do it by hand. When the kernel has no bbr, it first tries
   `modprobe tcp_bbr`; if there is still none, it falls back to `PRE_INSTALL_CC`, then cubic, then reno, and
   warns which one it used. **This applies to the running system only**: it writes and changes no file under
   `/etc/sysctl.d`, so after a reboot those files decide again, which it points out at the end.
2. **Removes the packages the install added**: only those recorded in `/etc/skyline-speeder/added-packages`
   (at install time, the difference between the installed sets before and after the package manager ran,
   including dependencies pulled in along the way and excluding packages the host already had; for apt, also
   intersected with the plan accepted for this request, which excludes packages installed by an
   unattended-upgrades run in parallel). On a host without this record — installed by hand, or installed before
   the installer started recording (before v0.3.0) — this step removes nothing. Guardrails:
   - `iproute2` (`iproute` and `iproute-tc` on Fedora/RHEL), `curl`, `ca-certificates` and `tar` (and on Alpine
     also `bash`) are never removed;
   - **nor is anything these packages depend on, recursively** — keep `curl` but remove the `libcurl4t64`
     under it, and apt's only resolution is "remove curl too", with the result that not a single package of the
     toolchain can be removed;
   - packages of dpkg priority `required`/`important` (judged per architecture) and dnf's protected packages
     are never removed;
   - a plan comes first: apt uses `apt-get -s --purge remove`; when the plan shows a `Purg`/`Remv` outside the
     list, the package depended upon is kept back on its own, with a warning saying who needs it, the rest stay
     on the list, and the plan is made again (at most three rounds); dnf is handled the same way with
     `rpm -e --test` (at most ten rounds), executed by `dnf remove` with `clean_requirements_on_remove` turned
     off; dnf's record takes only the transaction of this request and always excludes `gpg-pubkey`. apk runs
     `apk del` on the names the record added to `/etc/apk/world`, and apk itself keeps dependencies that other
     packages still need. When there is still no plan that stays within bounds, it removes **nothing at all**
     and prints the manual commands instead.
   - rustup is removed only when `/etc/skyline-speeder/added-rustup` exists. Any failure in this step is only a
     warning: by then the product itself has been uninstalled.

The N in "uninstalling will also remove N packages" in the guide at the end of an install is computed with the
same guardrails, and matches what an uninstall actually does. When nothing is installed on the host, it exits
with 0 and says `nothing of Skyline Speeder is installed here`; when only `/etc/skyline-speeder` is left, it
changes no sysctl or qdisc.

Criteria after uninstalling:

```sh
! command -v skyline-speederd >/dev/null
[ "$(sysctl -n net.ipv4.tcp_congestion_control)" = bbr ] && [ "$(sysctl -n net.core.default_qdisc)" = fq ]
```

(When the kernel has no bbr, use the algorithm named in the warning in the second line instead; with
`--restore-pre-install`, the values in the snapshot.)

### 11.4 `--restore-pre-install`

Replaces step 1 of section 11.3 with a restore from `/etc/skyline-speeder/pre-install-state`: the congestion
control and `default_qdisc`; when the snapshot records NICs and their root qdiscs, it restores the **kind** of
each NIC's root qdisc — only when it is still the `fq`/`mq/fq` this project left there, and it restores only
the kind, with default parameters (a `cake`'s bandwidth setting, for example, does not come back); an `mq` with
a mix of qdiscs under it only gets a warning, not a restore; when the two lists differ in length, no NIC is
restored. To restore `mq/<kind>`, it temporarily sets `default_qdisc` to that kind and runs
`tc qdisc del dev <nic> root` so that the kernel rebuilds its own `mq` from it (running `replace root mq` on an
`mq` the guard left, built by `tc`, is a no-op); only when the delete fails (the kernel's own `mq` with
handle 0) does it run `tc qdisc replace dev <nic> root mq`; then it sets `default_qdisc` back and reads
everything again to check. Without a snapshot (a manually installed host) it only warns, and the cc and qdiscs
stay as they are. The snapshot belongs to the current installation: after an earlier `--uninstall` and a
reinstall, it describes the host as it was just before that reinstall (section 2.4).

### 11.5 Uninstalling without the installer

```bash
ssctl drain --timeout 60 || true
systemctl disable --now skyline-speeder-enable.service skyline-speederd.service
rm -f /etc/systemd/system/skyline-speederd.service /etc/systemd/system/skyline-speeder-enable.service \
      /usr/local/sbin/skyline-speederd /usr/local/bin/ssctl
rm -rf /opt/skyline-speeder
systemctl daemon-reload
```

OpenRC: run `rc-service <service> stop` and `rc-update del <service> default` for `skyline-speeder-enable` and
then for `skyline-speederd`, then delete the two scripts under `/etc/init.d/`. Change the congestion control and
qdiscs back yourself.

### 11.6 Known behaviour: the struct_ops does not disappear before its connections end

After unregistering or uninstalling, `bpftool struct_ops show` may still list `skyline_cc` (`skyline_cc_txs` on
kernels ≥ 7.1). **This is not a failure**: the kernel does not free a struct_ops map while any socket still
references it, the same kind of reference counting as `rmmod` failing on a kernel module that is "in use". The
criteria (the kernel's registry itself is clean):

```bash
# should print exactly 1 (enabled) or 0 (disabled), never a duplicate registration because of a leftover map
sysctl -n net.ipv4.tcp_available_congestion_control | tr ' ' '\n' | grep -c '^skyline_cc$'
ss -tin | grep -c skyline_cc       # number of connections still referencing it
```

The kernel reclaims the leftover map after these connections end naturally, or after a reboot. **Do not**
force-detach a struct_ops that live sockets are still using.

---

## 12. OpenRC hosts (Alpine)

The one-line install applies as usual; these are the only differences:

- **It installs the musl artifact** (section 2.1), plus the `libelf`, `zlib`, `zstd-libs` and `libgcc` it
  needs at run time; the two OpenRC scripts are in the artifact too. A version without a musl build, such as
  `--release v0.4.0`, aborts outright. A source build takes about 10 minutes on 1 vCPU: Rust's musl target
  links statically by default, but Alpine keeps zlib's static library in `zlib-static`, which nobody installs,
  so linking would fail with `cannot find -lz`; the repository's `.cargo/config.toml` therefore makes builds on
  musl link dynamically.
- **The two services** are the scripts under `packaging/openrc/`, installed to `/etc/init.d/`, one for each of
  the two systemd units:
  - `skyline-speederd` is supervised by `supervise-daemon`: after an abnormal exit it is restarted 2 seconds
    later, and after 5 failures within 60 seconds it gives up (the counterpart of `Restart=on-failure`); on stop
    it sends SIGTERM first and allows 30 seconds for the struct_ops to be unregistered; output goes through
    `logger` into syslog (`/var/log/messages` on Alpine); before start it creates `/run/skyline-speeder` (the
    counterpart of `RuntimeDirectory=`, which **must be the parent directory of `socket_path`, `events_path`
    and `state_path`**), `/sys/fs/bpf/skyline-speeder` and `/sys/fs/cgroup/skyline-speeder`.
  - `skyline-speeder-enable`: `need skyline-speederd`; start runs `boot-enable.sh`, stop runs
    `boot-disable.sh`. The effect is the same as systemd's `Requires=` + `After=`, but OpenRC restarts it in the
    **background**: when `rc-service skyline-speederd restart` returns, `boot-enable.sh` may still be waiting
    for the socket, so wait for it to finish before checking (the installer waits on an upgrade).
- **cgroup v2**: on Alpine only OpenRC's `cgroups` service mounts it, and that service is in no runlevel by
  default. `skyline-speederd` declares `need cgroups`, so OpenRC starts it first on every boot; at install time
  the installer starts it once itself. When `rc_cgroup_mode` in `/etc/rc.conf` is set to `hybrid`/`legacy`, v2
  is not at this path, and the installer aborts and says so.
- Alpine has no `sudo` by default: run as root; the guide the installer prints at the end already has the
  commands written for this host.

| systemd | OpenRC |
|---|---|
| `systemctl restart skyline-speederd` | `rc-service skyline-speederd restart` |
| `systemctl enable --now skyline-speeder-enable.service` | `rc-update add skyline-speeder-enable default && rc-service skyline-speeder-enable start` |
| `systemctl disable --now skyline-speeder-enable.service` | `rc-service skyline-speeder-enable stop && rc-update del skyline-speeder-enable default` |
| `systemctl stop <service>` | `rc-service <service> stop` |
| `systemctl is-active <service>` (G1) | `rc-service <service> status` (prints `status: started`) |
| `systemctl is-enabled <service>` (G6) | `test -e /etc/runlevels/default/<service>` |
| `journalctl -u skyline-speederd.service` | `grep skyline-speederd /var/log/messages` |

---

## 13. Observability and troubleshooting

### 13.1 Fault quick reference

| Symptom | Look first at |
|---|---|
| The install fails at some step | The `error` line on screen and the tail of the log; the full output is in `/var/log/skyline-speeder-install.log` (section 0.3). It then asks whether to send an error report (section 13.6) |
| `Help us fix this? Send an error report ...` after an `ssctl` command | skyline-speederd recorded an error it stopped on, or the command failed in a way worth reporting (section 13.6) |
| `Error: configured fallback congestion control is unavailable` | Section 13.4 |
| The verifier rejects `skyline_cc` with an error pointing at the argument count | The kernel is an old branch such as 6.1/6.6, which is unsupported (section 1.3) |
| `ssctl` reports `connect to /run/skyline-speeder/speeder.sock` | The daemon did not come up or is still starting (section 5.1); `journalctl -u skyline-speederd.service` |
| The status is `○ STANDBY` | Not attached: `systemctl enable --now skyline-speeder-enable.service` or `ssctl enable` (section 5) |
| The status is `○ ATTACHED, NOT DEFAULT` | The default congestion control has been changed away: see the DRIFT GUARD section and the `guard:` log lines (section 9) |
| The status is `○ DRAINING` | A drain was run and is still waiting for existing connections (always so over SSH, section 11.1): to resume acceleration, `ssctl enable` |
| Not attached after a reboot | The enable unit is not enabled: G6 (section 6) |
| `rack_rto.stats` stays at 0 | The diagnosis table in section 7 |
| The NIC's qdisc is not `fq` | `notes` in the DRIFT GUARD section (section 9) |
| `drain` returns non-zero | Over SSH, the expected timeout (section 11.1) |
| `bpftool` still sees `skyline_cc` after uninstalling | Section 11.6 |
| Connections stall for tens of seconds, large files break off, 4K video stalls but 2K does not | `docs/usage.md`, "Appendix: how to investigate video stalls / interrupted large-file transfers" |
| Wrong version | `skyline-speederd --version` (disk) differs from `sv version` (memory) = not yet restarted into the new version (section 0.2) |

### 13.2 Reports, counters and the state file

- `ssctl status`: whether it is attached, the host-wide default, the guard, kernel capabilities, global
  sysctls, daemon uptime; `ssctl flows`: the accelerated connections, the parameters in force and first-flight
  redundancy, the algorithm's decision counters (the WHAT THE ALGORITHM DID section, `status.metrics` in
  `--json`). `metrics` is `null` when no struct_ops is loaded — never attached, after a drain has completed,
  after a daemon restart — and every new attach counts from zero. For all fields see
  `docs/02-interface-reference.md` sections 4 and 4.1.
- Those counters **accumulate from the attach**. The two `last …` lines at the end of the WHAT THE ALGORITHM
  DID section are the same ratios over roughly the last minute (`status.metrics_recent` in `--json`), to tell
  "still happening now" from "happened at some point since the attach".
  `guardrail trips` is the share of checked rounds in which the queueing-delay/ECN guardrail tripped, and
  `cwnd at the cap` is the share of all ACKs on which cwnd sat at `max_cwnd_packets` — the latter is the cap at
  work, not congestion. 0.4.x combined the two into one `guardrail hits`, both divided by the ACK count, so you
  cannot tell which one it was; to separate them on 0.4.x, count the `"type":9` lines in the event log
  (guardrail trips, section 13.3), and the rest are ACKs at the cap.
- `/run/skyline-speeder/state.json` is rewritten only when the daemon starts and after each successfully
  handled request; it is **not** live state. For monitoring, call `ssctl status --json`.
- With `ssctl snapshot <path>` the daemon writes the file, and a relative path is resolved against the
  daemon's working directory; it writes only regular files and refuses a target that is a symbolic link,
  directory, device or FIFO (since 0.5.2). On systemd hosts the daemon runs under
  `ProtectSystem=strict` with a private `/tmp`: most paths are read-only to it, and a file it writes into
  `/tmp` is invisible to you; writing under `/run/skyline-speeder/` is the safest choice.

### 13.3 Event log

The event ring buffer is persisted to `runtime.events_path` (default `/run/skyline-speeder/events.jsonl`), one
JSON object per line, recording state transitions, guardrail trips, configuration generation switches and so
on; for fields and event codes see `docs/02-interface-reference.md` section 8. It lives in `/run` (an in-memory
tmpfs), and its size is limited by `runtime.events_max_mib` (default 8 MiB; when full it rotates to
`events.jsonl.1`, so it takes at most about twice that); set it to `0` to turn the event log off. Versions
before 0.2.0 have no such limit and can fill `/run`, making services that depend on `/run`, such as Docker,
fail: on such versions reclaim the space with `truncate -c -s 0 /run/skyline-speeder/events.jsonl`, not `rm` —
the daemon still holds the file handle, so after a delete the space is not freed.

### 13.4 `configured fallback congestion control is unavailable`

Validation (the installer's *Running the kernel verifier* step, `--validate-only`, `ssctl enable`) stopped
before loading any BPF object: this kernel neither has the algorithm named in `fallback_cc` registered nor a
loadable module `tcp_<name>` for it, and the capability report's `notes` lists the algorithms registered on
this host. Change `fallback_cc` in `/etc/skyline-speeder/speeder.toml` to one of them (usually `bbr`, which the
host was using), then rerun the install command or `systemctl restart skyline-speederd`. Versions before 0.4.3
accept only **registered** algorithms: on kernels such as xanmod that default to bbr and build cubic as a
module, the template's `cubic` is not registered after boot, so the install failed; change it to `bbr` in the
same way. Do not work around it with `modprobe tcp_cubic`: the daemon probes only once, at start, and after a
reboot cubic is not loaded, so the automatic attach at boot fails.

### 13.5 Dual-stack listeners and address families

IPv4 connections on a dual-stack listening socket appear as IPv4-mapped `AF_INET6` (`::ffff:a.b.c.d`), not as
plain `AF_INET` — this is normal for most services that do not bind an address family explicitly, not an
anomaly. Skyline Speeder's address-family checks accept both forms.

### 13.6 Error reports

Nothing is ever sent without a `y`. The question comes after these, once each:

| Where | When |
|---|---|
| `install.sh` | An install, upgrade or `--uninstall` fails (not `--check`, not a refusal before anything ran such as "must run as root", not Ctrl-C) |
| `ssctl` | `enable` is refused; the daemon cannot be reached and its service manager has given up on it (`failed`, OpenRC's `crashed`); a reply does not decode |
| `ssctl` | skyline-speederd recorded an error it stopped on (`main` returned an error) or a panic in any thread, in `/var/lib/skyline-speeder/reports/` -- asked about after the command's own output, on the next `ssctl` run on a terminal |

It is asked only on a terminal: `install.sh` asks on `/dev/tty`, `ssctl` only when it runs as root without
`--json` and both stdin and stderr are terminals. Answers: `y` sends, `v` shows every field that would be sent and
asks again, anything else (or five minutes without an answer) sends nothing. A daemon report is removed once it
is sent or declined, so it is asked about once; one that could not be sent stays for the next time. A report
`install.sh` offers takes the daemon's reports along with it. The daemon records at most 10 distinct reports; the
same failure on every restart adds to one report's `count`.

`SKYLINE_FEEDBACK` decides in advance, for both tools (`ssctl` only as root): `no` never asks, `yes` sends without
asking (also with no terminal), unset or `ask` asks. Through sudo: `sudo SKYLINE_FEEDBACK=no ./install.sh`.

A report is one Sentry event per error, sent with `curl` over HTTPS (TLS 1.2 or newer). It carries: the error and
its causes, the failing step (install) or the command (`ssctl`), the version, `ID`, `VERSION_ID` and `PRETTY_NAME`
from `/etc/os-release`, the kernel release, the architecture, the C library, the package and service manager, the
service state, and the last 40 lines of the failing step's install log or of the daemon's log (`journalctl -u
skyline-speederd.service -o cat`, or its lines in `/var/log/messages` under OpenRC). Before it is shown or sent,
IPv4, IPv6 and MAC addresses, e-mail addresses, credentials in URLs, GitHub and Sentry tokens, the values of
`token=`, `secret=` and `password=` pairs, `/home/<user>`, the host name and the invoking user's name are replaced
by placeholders such as `<ipv4>` and `<host>`; it errs towards removing too much (a four-part version number reads
as an address). No host name, machine ID or user identity is sent.

The Sentry DSN is compiled into `ssctl` and written into `install.sh` only encrypted (ChaCha20). The key is in the
same code, as it has to be for the tools to use the DSN, so this keeps the DSN away from scanners rather than
secret; a DSN can only submit events. `infra/seal-feedback-dsn.py` seals a new one; with none sealed, nothing asks
and nothing is sent.

A daemon report, `/var/lib/skyline-speeder/reports/<hash>.json`, one JSON object:

```json
{"version":"<skyline-speederd version>","kind":"exited","message":"<error with its causes>","first_seen":1793000000,"last_seen":1793000100,"count":4}
```

---

## 14. Related documents

| Document | Contents |
|---|---|
| [README.md](README.md) / [README.zh.md](README.zh.md) | In the order you get started: what it is, the kernel requirement, quick start, how it works, configuration (every feature, and every line of `ssctl status` / `ssctl flows`), measured performance |
| [docs/usage.md](docs/usage.md) | For newcomers: every switch and parameter, the four preset recipes, dynamic RTO, DSCP, troubleshooting stalls |
| [docs/02-interface-reference.md](docs/02-interface-reference.md) | `ssctl` commands, the wire protocol, status fields, configuration fields, event codes, guard rules |
| [docs/03-design.md](docs/03-design.md) | The design of each module and its trade-offs |
| [docs/04-performance-report.md](docs/04-performance-report.md) | Test-bed data, validity boundaries and limitations (section 8) |
| [research/experiments/README.md](research/experiments/README.md) | Reproducing the performance tests in your own environment |
| [research/experiments/single-ended/](research/experiments/single-ended/) | The field comparison on a single public Internet path cited in the README's *Measured performance*: method, per-round data, raw records and scripts (not test-bed data) |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Building, the verifier, hard invariants, the release process |
