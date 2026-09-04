#!/bin/sh
# Build the release artifacts with Cargo and install them.
#
# The workspace is pure Rust, so the whole install surface is two private
# executables, one generated D-Bus service file, and two portal metadata
# files. This script replaces the former Meson packaging layer; run it
# directly or from a distribution package recipe.
#
# Usage:
#   ./scripts/install.sh [--prefix PREFIX] [--libexecdir DIR] [--datadir DIR]
#                        [--no-build]
#
#   --prefix PREFIX   Installation prefix (default /usr)
#   --libexecdir DIR  Directory under the prefix for private executables
#                     (default libexec)
#   --datadir DIR     Directory under the prefix for architecture-independent
#                     data (default share)
#   --no-build        Skip the cargo build and install from target/release
#                     as-is (the build must already exist)
#
# Honors DESTDIR for staged distribution packaging; the generated D-Bus
# Exec path records the final (staged) path, without DESTDIR:
#
#   DESTDIR="$PWD/stage" ./scripts/install.sh --prefix /usr
set -eu

usage() {
    sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

prefix=/usr
libexecdir=libexec
datadir=share
build=yes

while [ $# -gt 0 ]; do
    case $1 in
        --prefix) prefix=${2:?}; shift 2 ;;
        --libexecdir) libexecdir=${2:?}; shift 2 ;;
        --datadir) datadir=${2:?}; shift 2 ;;
        --no-build) build=no; shift ;;
        --help|-h) usage 0 ;;
        *) printf 'unknown option: %s\n' "$1" >&2; usage 1 ;;
    esac
done

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
case $libexecdir in /*) libexec=$libexecdir ;; *) libexec=$prefix/$libexecdir ;; esac
case $datadir in /*) data=$datadir ;; *) data=$prefix/$datadir ;; esac

if [ "$build" = yes ]; then
    cargo build \
        --manifest-path "$repo/Cargo.toml" \
        --locked --release \
        -p xdg-desktop-portal-atrium \
        -p atrium-portal-prompter
fi
target=$repo/target/release

stage=${DESTDIR:-}
install -Dm755 "$target/xdg-desktop-portal-atrium" \
    "$stage$libexec/xdg-desktop-portal-atrium"
install -Dm755 "$target/atrium-portal-prompter" \
    "$stage$libexec/atrium-portal-prompter"

# The D-Bus activation Exec path is resolved through the *final* prefix,
# never through DESTDIR, or the session bus would try to launch the file
# inside the staging tree.
service=$stage$prefix/$datadir/dbus-1/services/org.freedesktop.impl.portal.desktop.atrium.service
service_tmp=$(mktemp)
trap 'rm -f "$service_tmp"' EXIT
sed "s|@libexecdir@|$libexec|" \
    "$repo/contrib/dbus-1/services/org.freedesktop.impl.portal.desktop.atrium.service.in" \
    > "$service_tmp"
install -Dm644 "$service_tmp" "$service"

install -Dm644 "$repo/contrib/xdg-desktop-portal/portals/atrium.portal" \
    "$stage$data/xdg-desktop-portal/portals/atrium.portal"
install -Dm644 "$repo/contrib/xdg-desktop-portal/atrium-portals.conf" \
    "$stage$data/xdg-desktop-portal/atrium-portals.conf"

printf 'installed:\n'
printf '  %s\n' \
    "$libexec/xdg-desktop-portal-atrium" \
    "$libexec/atrium-portal-prompter" \
    "$data/dbus-1/services/org.freedesktop.impl.portal.desktop.atrium.service" \
    "$data/xdg-desktop-portal/portals/atrium.portal" \
    "$data/xdg-desktop-portal/atrium-portals.conf"
[ -z "$stage" ] || printf 'staged under %s\n' "$stage"
