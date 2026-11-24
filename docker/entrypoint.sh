#!/bin/sh
# OMT discovery on Linux is DNS-SD through avahi-daemon (D-Bus). OMT_AVAHI:
#   internal  start dbus and avahi-daemon in the container (as root), then run
#             the command as RUN_AS (default 1000:1000). Needs host
#             networking for mDNS to reach the LAN.
#   host      use the host's avahi-daemon: mount /run/dbus (and
#             /run/avahi-daemon) from the host.
#   off       no DNS-SD: connect with omt://host:port, or set
#             OMT_DISCOVERY_SERVER (an OMT discovery server).
set -eu
mode="${OMT_AVAHI:-internal}"
run_as="${RUN_AS:-1000:1000}"
storage="${OMT_STORAGE_PATH:-/config/omt}"
if [ "$(id -u)" = 0 ]; then
    mkdir -p "$storage"
    chown "$run_as" "${OMT_STATE_DIR:-/config}" "$storage" 2>/dev/null || true
    if [ "$mode" = internal ]; then
        mkdir -p /run/dbus /run/avahi-daemon
        rm -f /run/dbus/pid /run/avahi-daemon/pid
        dbus-daemon --system --fork
        # shellcheck disable=SC2086
        avahi-daemon --daemonize --no-chroot ${AVAHI_ARGS:-}
        for _ in 1 2 3 4 5 6 7 8 9 10; do avahi-daemon --check 2>/dev/null && break; sleep 0.3; done
    fi
    exec setpriv --reuid="${run_as%:*}" --regid="${run_as#*:}" --clear-groups -- "$@"
fi
mkdir -p "$storage" 2>/dev/null || true
exec "$@"
