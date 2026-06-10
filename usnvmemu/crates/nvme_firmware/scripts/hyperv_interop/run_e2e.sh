#!/bin/bash
# WSL entry point for the pcie_remote/OpenHCL Hyper-V e2e harness.
#
# Single command that: (optionally) cross-builds + stages the current
# nvme_firmware.exe, drives the Windows-side e2e (deploy-or-reuse VM, start
# controller, guest format + 4 MiB IO), then runs an INDEPENDENT host-side byte
# oracle. This is the pcie_remote counterpart to vfio_user_transport's
# qemu_interop/run_qemu_vfio.py and nvme_of_tcp_target's interop_py.
#
# Two oracles must agree for PASS:
#   1. guest-side  : 4 MiB write embedding $MARKER, flush, read back, compare.
#   2. host-side   : raw byte-scan of the backing file for $MARKER (done by the
#                    ps1 after stopping the controller, whose mmap holds the file
#                    lock). A different code path than the guest readback; catches
#                    NTFS-cache false positives.
#
# Usage:
#   usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/run_e2e.sh
# Env (all optional):
#   BUILD=1            cross-build + stage nvme_firmware.exe first (default 0: reuse staged)
#   FORCE_DEPLOY=1     recreate the VM from IGVM+VHDX (default 0: reuse/restart existing)
#   WIN_WORKDIR=...    WSL path to the Windows staging dir (default /mnt/c/temp/pcie_remote_exp)
#   VM_NAME=...        (default pcie-remote-exp)
#   ADMIN_USER/ADMIN_PASS  guest creds (default Administrator / PcieRemote123!)
#
# Prereq: a provisioned guest.vhdx + an OpenHCL pcie_remote IGVM staged in
# WIN_WORKDIR, plus the one-time Hyper-V setup in HYPERV_RUNBOOK.md.
set -uo pipefail
HERE="$(realpath "$(dirname "${BASH_SOURCE[0]}")")"
WIN_WORKDIR="${WIN_WORKDIR:-/mnt/c/temp/pcie_remote_exp}"
VM_NAME="${VM_NAME:-pcie-remote-exp}"
ADMIN_USER="${ADMIN_USER:-Administrator}"
ADMIN_PASS="${ADMIN_PASS:-PcieRemote123!}"
PS='/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe'
MARKER="USNVMEMU-OPENHCL-E2E-$(date +%Y%m%d%H%M%S)"

# Never leave the --retries 0 controller running if we're interrupted. NB: stops
# ALL nvme_firmware.exe by name — fine for a dedicated dev e2e box.
cleanup_controller() {
    "$PS" -NoProfile -NonInteractive -Command "Get-Process nvme_firmware -EA SilentlyContinue | Stop-Process -Force" >/dev/null 2>&1
}
trap cleanup_controller INT TERM

if [ "${BUILD:-0}" = "1" ]; then
    echo "== build + stage current nvme_firmware.exe =="
    "$HERE/build_and_stage.sh" "$WIN_WORKDIR" || { echo "build failed"; exit 1; }
fi

# stage the committed driver into the Windows workdir for -File execution
if [ ! -d "$WIN_WORKDIR" ]; then echo "WIN_WORKDIR missing: $WIN_WORKDIR (run with BUILD=1 to stage first)"; exit 1; fi
WORKDIR_WIN="$(wslpath -w "$WIN_WORKDIR")"
cp -f "$HERE/run_hyperv_nvme_e2e.ps1" "$WIN_WORKDIR/run_hyperv_nvme_e2e.ps1" || { echo "failed to stage ps1 into $WIN_WORKDIR"; exit 1; }
PS1_WIN="$WORKDIR_WIN\\run_hyperv_nvme_e2e.ps1"

echo "== driving Hyper-V e2e (vm=$VM_NAME marker=$MARKER) =="
DEPLOY_ARG=""; [ "${FORCE_DEPLOY:-0}" = "1" ] && DEPLOY_ARG="-ForceDeploy"
timeout 900 "$PS" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PS1_WIN" \
    -VmName "$VM_NAME" -WorkDir "$WORKDIR_WIN" -Marker "$MARKER" \
    -AdminUser "$ADMIN_USER" -AdminPass "$ADMIN_PASS" $DEPLOY_ARG > /tmp/hyperv_e2e_drive.log 2>&1
PS_RC=$?
sed -E 's/\x1b\[[0-9;]*m//g' /tmp/hyperv_e2e_drive.log

cleanup_controller   # stop the --retries 0 controller

# The ps1 gates RESULT=PASS on BOTH oracles: the guest readback AND a raw-backing
# byte scan it runs after stopping the controller (which releases the mmap file
# lock; the guest's NVMe FLUSH already made the bytes durable). Verdict mirrors it.
if grep -q "RESULT=PASS" /tmp/hyperv_e2e_drive.log; then
    echo "E2E PASS (guest readback oracle + independent raw-backing oracle agree)"
    exit 0
else
    echo "E2E FAIL (ps_rc=$PS_RC) — see /tmp/hyperv_e2e_drive.log"
    exit 1
fi
