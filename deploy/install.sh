#!/bin/sh
# Run manually on the monitored server from an unpacked trusted release.
set -eu
if [ "$(id -u)" -ne 0 ]; then
    echo 'Run as root: sudo sh deploy/install.sh [path-to-oyasys-binary]' >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo 'Python 3 with sqlite3 support is required for on-demand updates.' >&2
    exit 1
fi
python3 -I -c 'import sqlite3, ssl; assert __import__("sys").version_info >= (3, 9)' || {
    echo 'Python 3.9+ with sqlite3 and SSL support is required.' >&2
    exit 1
}
release_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
binary=${1:-"$release_root/oyasys"}
if [ ! -f "$binary" ]; then
    echo "Binary not found: $binary" >&2
    exit 1
fi
if systemctl is-active --quiet oyasys.service; then
    echo 'Stop oyasys.service before updating, then rerun this installer.' >&2
    exit 1
fi
if ! command -v flock >/dev/null 2>&1; then
    echo 'The util-linux flock command is required to serialize installation and updates.' >&2
    exit 1
fi
watcher_was_active=0
if systemctl is-active --quiet oyasys-update.path; then watcher_was_active=1; fi
trap 'if [ "$watcher_was_active" -eq 1 ]; then systemctl start oyasys-update.path || true; fi' 0
# Serialize with the on-demand helper before changing installed files.
if systemctl is-active --quiet oyasys-update.service; then
    echo 'An update is running; wait for it to finish before reinstalling.' >&2
    exit 1
fi
if systemctl cat oyasys-update.path >/dev/null 2>&1; then
    systemctl stop oyasys-update.path
fi
if systemctl is-active --quiet oyasys-update.service; then
    echo 'An update started while disabling its trigger; wait before reinstalling.' >&2
    exit 1
fi
if [ -e /var/lib/oyasys-updater/inbox/request.json ] || [ -L /var/lib/oyasys-updater/inbox/request.json ]; then
    echo 'An update request is queued; restart oyasys-update.path and let it finish before reinstalling.' >&2
    exit 1
fi
if [ -e /var/lib/oyasys-updater/transaction.json ] || [ -L /var/lib/oyasys-updater/transaction.json ]; then
    echo 'An interrupted update needs recovery. Run oyasys-update.service and inspect status.json before reinstalling.' >&2
    exit 1
fi
if ! getent group oyasys >/dev/null; then
    groupadd --system oyasys
fi
if ! id oyasys >/dev/null 2>&1; then
    useradd --system --gid oyasys --home-dir /var/lib/oyasys --shell /usr/sbin/nologin oyasys
fi
if [ "$(id -u oyasys)" -eq 0 ]; then
    echo 'The oyasys service account must not be root.' >&2
    exit 1
fi
python3 -I - <<'PY'
import os, stat
for name in ['/opt/oyasys', '/etc/oyasys', '/var/lib/oyasys-updater']:
    if os.path.lexists(name):
        item = os.lstat(name)
        if not stat.S_ISDIR(item.st_mode) or item.st_uid != 0 or item.st_mode & 0o022:
            raise SystemExit('Refusing unsafe existing installation directory: ' + name)
for name in ['/var/lib/oyasys', '/var/lib/oyasys-updater/inbox']:
    if os.path.lexists(name) and not stat.S_ISDIR(os.lstat(name).st_mode):
        raise SystemExit('Refusing linked/non-directory service data: ' + name)
PY
install -d -m 0755 /opt/oyasys /etc/oyasys
install -d -m 0750 -o oyasys -g oyasys /var/lib/oyasys
install -d -m 0755 -o root -g root /var/lib/oyasys-updater
install -d -m 0700 -o oyasys -g oyasys /var/lib/oyasys-updater/inbox
# Same advisory lock as the helper; keep it until this installer exits.
exec 9>>/var/lib/oyasys-updater/lock
if ! flock -n 9; then
    echo 'The updater owns its lock; wait before reinstalling.' >&2
    exit 1
fi
if [ -e /var/lib/oyasys-updater/transaction.json ] || [ -L /var/lib/oyasys-updater/transaction.json ]; then
    echo 'An interrupted transaction appeared; recover it before reinstalling.' >&2
    exit 1
fi
install -m 0755 "$binary" /opt/oyasys/oyasys.new
mv /opt/oyasys/oyasys.new /opt/oyasys/oyasys
install -m 0644 "$release_root/deploy/oyasys.service" /etc/systemd/system/oyasys.service
install -m 0755 "$release_root/deploy/update.py" /opt/oyasys/update.py
install -m 0644 "$release_root/deploy/oyasys-update.service" /etc/systemd/system/oyasys-update.service
install -m 0644 "$release_root/deploy/oyasys-update.path" /etc/systemd/system/oyasys-update.path
if [ ! -e /etc/oyasys/environment ]; then
    install -m 0644 "$release_root/deploy/environment.example" /etc/oyasys/environment
fi
if [ ! -e /var/lib/oyasys-updater/status.json ]; then
    printf '%s\n' '{"request_id":"","state":"idle","latest_version":null,"message":"Ready for an explicit update check","checked_at":null}' > /var/lib/oyasys-updater/status.json
    chmod 0644 /var/lib/oyasys-updater/status.json
fi
systemctl daemon-reload
systemctl enable oyasys-update.service
systemctl enable --now oyasys-update.path
watcher_was_active=0
echo 'Installed. Existing data and configuration retained; service not started.'
echo 'Configure /etc/oyasys/environment and network access, then run:'
echo '  sudo systemctl enable --now oyasys.service'
