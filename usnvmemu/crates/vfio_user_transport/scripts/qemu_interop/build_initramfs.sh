#!/bin/bash
# Build the minimal initramfs (busybox + /init = guest_init.sh) for the
# vfio-user guest-boot e2e harness. Output defaults to a cache dir; the busybox
# static binary is fetched once and cached. The guest kernel is NOT built here —
# the harness uses the host's WSL2 kernel (nvme + devtmpfs built-in) by default.
#
# Usage: build_initramfs.sh [OUTPUT_CPIO_GZ]
set -euo pipefail
HERE="$(realpath "$(dirname "${BASH_SOURCE[0]}")")"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/usnvmemu_vfio_guest"
OUT="${1:-$CACHE/initramfs.cpio.gz}"
BB_URL="https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox"
mkdir -p "$CACHE"

BB="$CACHE/busybox"
if [ ! -x "$BB" ]; then
    echo "== fetching busybox-static =="
    curl -fsSL -o "$BB" "$BB_URL"
    chmod +x "$BB"
fi

command -v gzip >/dev/null || { echo "missing host tool: gzip" >&2; exit 1; }

ROOT="$(mktemp -d)"
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT"/{bin,proc,sys,dev,tmp}
cp "$BB" "$ROOT/bin/busybox"
chmod +x "$ROOT/bin/busybox"
cp "$HERE/guest_init.sh" "$ROOT/init"
chmod +x "$ROOT/init"

# nvme driver modules (from fetch_guest_kernel.sh) — the guest kernel ships nvme
# as a module, so init insmods these. Skipped if not fetched yet.
if ls "$CACHE"/modules/*.ko >/dev/null 2>&1; then
    mkdir -p "$ROOT/modules"
    cp "$CACHE"/modules/*.ko "$ROOT/modules/"
fi

# use busybox's own cpio applet (host cpio is often absent and apt needs sudo)
( cd "$ROOT" && find . | "$BB" cpio -o -H newc 2>/dev/null | gzip -9 ) > "$OUT"
echo "== initramfs: $OUT ($(stat -c %s "$OUT") bytes) =="
