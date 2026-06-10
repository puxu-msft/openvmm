#!/bin/bash
# Fetch a real guest kernel (bzImage) + nvme modules for the vfio-user guest-boot
# harness. The WSL2 host kernel is a raw vmlinux ELF (QEMU -kernel rejects it),
# and building one here is blocked (no flex/bison, no sudo), so we extract a 6.8
# bzImage + nvme modules from Ubuntu packages (nvme is a module in Ubuntu kernels).
# Any nvme-capable bzImage works — set GUEST_KERNEL to skip this script.
#
# No sudo needed: just downloads .debs and unpacks them with ar/tar/zstd.
set -euo pipefail
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/usnvmemu_vfio_guest"
mkdir -p "$CACHE"; cd "$CACHE"

for t in ar tar zstd file curl; do command -v "$t" >/dev/null || { echo "missing host tool: $t" >&2; exit 1; }; done

BASE="http://archive.ubuntu.com/ubuntu/pool/main/l/linux"
IMG="linux-image-unsigned-6.8.0-31-generic_6.8.0-31.31_amd64.deb"
MOD="linux-modules-6.8.0-31-generic_6.8.0-31.31_amd64.deb"
for d in "$IMG" "$MOD"; do
    [ -f "$d" ] || { echo "== fetch $d =="; curl -fsSL -o "$d" "$BASE/$d"; }
done

echo "== extract bzImage =="
ar p "$IMG" data.tar | tar -x --wildcards './boot/vmlinuz-*'
cp boot/vmlinuz-* vmlinuz
file vmlinuz | grep -q bzImage || { echo "extracted kernel is not a bzImage" >&2; exit 1; }

echo "== extract + decompress nvme modules (.ko.zst -> raw .ko for busybox insmod) =="
rm -rf lib modules; mkdir -p modules
ar p "$MOD" data.tar | tar -x --wildcards \
  './lib/modules/*/kernel/drivers/nvme/host/nvme.ko.zst' \
  './lib/modules/*/kernel/drivers/nvme/host/nvme-core.ko.zst' \
  './lib/modules/*/kernel/drivers/nvme/common/*.ko.zst'
find lib/modules -name '*.ko.zst' | while read -r f; do zstd -dcq "$f" > "modules/$(basename "${f%.zst}")"; done

echo "== ready =="
echo "  kernel : $CACHE/vmlinuz"
echo "  modules: $(ls modules/ | tr '\n' ' ')"
