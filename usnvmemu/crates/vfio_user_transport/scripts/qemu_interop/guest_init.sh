#!/bin/busybox sh
# vfio-user guest-boot e2e — runs INSIDE a real QEMU guest (WSL2 kernel + this
# busybox initramfs). Proves the crate's vfio-user *server* lets a real guest
# kernel's nvme driver bind to the device AND do real NVMe IO — not just realize.
#
# The per-run marker arrives via the kernel cmdline (`gmarker=...`); the host
# side (run_qemu_vfio_guest.py) scans the raw backing file for the same marker —
# an INDEPENDENT oracle (raw host file vs the guest's nvme readback).
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null

# load the nvme driver (the guest kernel ships it as a module); dep order is
# nvme-keyring <- nvme-auth <- nvme-core <- nvme; 2nd pass covers any ordering.
if [ -d /modules ]; then
    for m in nvme-keyring nvme-auth nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
    for m in nvme-core nvme; do insmod /modules/$m.ko 2>/dev/null; done
fi

MARKER=$(sed -n 's/.*gmarker=\([^ ]*\).*/\1/p' /proc/cmdline)
echo "GUEST: cmdline marker=[$MARKER]"

# wait for the nvme namespace to enumerate
i=0
while [ $i -lt 100 ]; do [ -e /dev/nvme0n1 ] && break; sleep 0.1; i=$((i + 1)); done
echo "GUEST: nvme nodes:"; ls -l /dev/nvme* 2>/dev/null
if [ ! -e /dev/nvme0n1 ]; then echo "GUEST_RESULT=NO_NVME"; poweroff -f; fi

# write the marker to LBA 0 (padded to one sector), flush, read back, compare
printf '%s' "$MARKER" > /tmp/m
dd if=/tmp/m of=/dev/nvme0n1 bs=512 count=1 conv=fsync,sync 2>/dev/null
# a second, larger IO at LBA 8 to exercise a multi-page transfer (4 KiB)
dd if=/dev/zero bs=512 count=8 2>/dev/null | tr '\000' '\127' | dd of=/dev/nvme0n1 bs=512 seek=8 count=8 conv=fsync 2>/dev/null
sync
RB=$(dd if=/dev/nvme0n1 bs=512 count=1 2>/dev/null | head -c ${#MARKER})
echo "GUEST: readback=[$RB]"
if [ "$RB" = "$MARKER" ]; then
    echo "GUEST_RESULT=PASS marker=$MARKER"
else
    echo "GUEST_RESULT=FAIL_MISMATCH rb=[$RB]"
fi
poweroff -f
