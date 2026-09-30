#!/usr/bin/env bash
set -euo pipefail

CONFIRM=${1:-}
REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

# The service manager decides where the two services go: systemd units, or
# OpenRC scripts in /etc/init.d (Alpine). Same two services either way.
if [ -d /run/systemd/system ]; then
    INIT=systemd
elif command -v openrc-run >/dev/null 2>&1; then
    INIT=openrc
else
    INIT=
fi

if [ "$CONFIRM" != --confirm-install ]; then
    if [ "$INIT" = openrc ]; then
        SERVICES="  /etc/init.d/skyline-speederd
  /etc/init.d/skyline-speeder-enable"
    else
        SERVICES="  /etc/systemd/system/skyline-speederd.service
  /etc/systemd/system/skyline-speeder-enable.service"
    fi
    cat <<EOF
This installs or replaces:
  /usr/local/sbin/skyline-speederd
  /usr/local/bin/ssctl
  /opt/skyline-speeder/bpf/*.bpf.o
  /opt/skyline-speeder/infra/*.sh
$SERVICES
It creates /etc/skyline-speeder/speeder.toml only when that file does not already exist.

Re-run with: $0 --confirm-install
EOF
    exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
    echo "Installation requires root." >&2
    exit 1
fi
if [ -z "$INIT" ]; then
    echo "Neither systemd nor OpenRC runs this host; there is nothing to install the services into." >&2
    exit 1
fi

cd "$REPOSITORY_ROOT"
make bpf
cargo build --workspace --release

install -d /opt/skyline-speeder/bpf /opt/skyline-speeder/infra /etc/skyline-speeder /run/skyline-speeder /sys/fs/bpf/skyline-speeder \
    /usr/local/sbin /usr/local/bin
install -m 0755 target/release/skyline-speederd /usr/local/sbin/skyline-speederd
install -m 0755 target/release/ssctl /usr/local/bin/ssctl
install -m 0644 build/bpf/*.bpf.o /opt/skyline-speeder/bpf/
install -m 0755 infra/apply-guest-profile.sh infra/collect-guest-metrics.sh \
    infra/snapshot-skyline-events.sh infra/run-in-skyline-cgroup.sh \
    infra/boot-enable.sh infra/boot-disable.sh /opt/skyline-speeder/infra/
if [ "$INIT" = systemd ]; then
    install -m 0644 packaging/skyline-speederd.service /etc/systemd/system/skyline-speederd.service
    install -m 0644 packaging/skyline-speeder-enable.service \
        /etc/systemd/system/skyline-speeder-enable.service
else
    install -m 0755 packaging/openrc/skyline-speederd /etc/init.d/skyline-speederd
    install -m 0755 packaging/openrc/skyline-speeder-enable /etc/init.d/skyline-speeder-enable
fi
if [ ! -e /etc/skyline-speeder/speeder.toml ]; then
    install -m 0644 config/speeder-guest.toml /etc/skyline-speeder/speeder.toml
fi
# Under OpenRC nothing may have mounted cgroup v2 yet (on Alpine the cgroups
# service is in no runlevel; skyline-speederd's `need cgroups` starts it at
# boot), and /sys/fs/cgroup is then a plain sysfs directory that refuses mkdir.
if [ "$INIT" = openrc ] && [ ! -e /sys/fs/cgroup/cgroup.controllers ]; then
    rc-service cgroups start
fi
mkdir -p /sys/fs/cgroup/skyline-speeder
if [ "$INIT" = systemd ]; then
    systemctl daemon-reload
    systemctl enable skyline-speederd.service
else
    rc-update add skyline-speederd default
fi
# skyline-speeder-enable is installed but NOT enabled here: attaching the
# struct_ops changes the congestion control for every new connection on the box,
# which is an operator decision, not an install-time one. install.sh (the
# one-click path) enables it after confirming the runtime prerequisites.
echo "Installed Skyline Speeder."
echo "Review /etc/skyline-speeder/speeder.toml (especially runtime.tc_interface),"
if [ "$INIT" = systemd ]; then
    echo "then start skyline-speederd.service, or run ./install.sh for the guided path."
else
    echo "then run 'rc-service skyline-speederd start', or ./install.sh for the guided path."
fi
