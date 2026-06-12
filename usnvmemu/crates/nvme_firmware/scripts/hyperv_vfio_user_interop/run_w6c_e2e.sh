#!/bin/bash
# WSL entry point for the OpenHCL + vfio-user NVMe (W6b/W6c) Hyper-V e2e harness.
#
# This is the vfio-user-over-OpenHCL counterpart to:
#   - hyperv_interop/run_e2e.sh        (pcie_remote/vsock, nvme_firmware.exe on HOST)
#   - vfio_user_transport/scripts/qemu_interop  (vfio-user over QEMU)
#
# Topology here: usnvmemu (the nvme_firmware binary) runs as a vfio-user SERVER
# INSIDE VTL2; underhill_core's `vfio_user_pci_device` is the vfio-user CLIENT and
# presents the emulated NVMe controller to the VTL0 guest. The full data path:
#
#   guest stornvme.sys -> VPCI -> underhill vfio_user_pci_device (client)
#     -> vfio-user REGION_RW (MMIO) + DMA_MAP(/dev/mshv_vtl_low fd, SCM_RIGHTS)
#     -> usnvmemu (VTL2 server) zero-copy DMA of guest RAM -> backing file
#
# Two INDEPENDENT oracles must agree for PASS:
#   oracle-1 (guest)  : write 4 MiB embedding $MARKER, flush, read back, compare.
#   oracle-2 (host)   : raw byte-scan of the VTL2 backing file for $MARKER, via
#                       STREAMING `ohcldiag-dev file -p` piped to a HOST grep
#                       (different path than the guest NTFS readback).
#
# =====================================================================
# HARD-WON CORRECT METHODS baked into this harness (do NOT "simplify" back):
#  * VTL2 process launch/exec: `ohcldiag-dev <vm> run -- sh -c '...'`. The `--`
#    is REQUIRED: ohcldiag's own clap eats `-c` otherwise ("unexpected argument
#    '-c'"). Binary push uses base64 over `run --` stdin.
#  * Liveness in VTL2: use `ps | grep '[u]snvmemu'` (real evidence). Do NOT use
#    `pgrep -x usnvmemu` (busybox -x quirk -> false DEAD) NOR `pgrep -f
#    "/tmp/usnvmemu"` (the pattern matches the pgrep cmdline itself -> false ALIVE).
#  * oracle-2 backing scan: stream out with `file -p | <host grep>`. Do NOT run
#    `grep -a` on the binary backing INSIDE VTL2: a newline-free 256 MB file is
#    buffered as one giant "line" -> OOM in the 512 MB-RAM VTL2 -> oops=panic ->
#    reboot (finding-5). Streaming via file-p has no VTL2 buffering.
#  * Kill usnvmemu for the revive test: `pkill -f "/tmp/usnvmemu --vfio-user-sock"`
#    (by cmdline) or by pid. NOT `pkill -x usnvmemu` (busybox -x matches nothing).
#  * IGVM must be built WITH the vpci feature:
#    `cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci`
#    (default build lacks it -> underhill bail!("built without vpci support") ->
#    control_state stuck "starting"; finding-1).
#  * DMA fd_offset is the BARE guest GPA (mshv_vtl_low direct view), set by
#    underhill_mem sharing(); independent of the VTL0 alias map (W6c).
# =====================================================================
#
# Usage:
#   usnvmemu/crates/nvme_firmware/scripts/hyperv_vfio_user_interop/run_w6c_e2e.sh
# Env (all optional):
#   BUILD_FW=1     cross-build the usnvmemu musl binary first (default 0: reuse)
#   WIN_WORKDIR=   WSL path to Windows staging dir (default /mnt/c/temp/pcie_remote_exp)
#   VM_NAME=       (default pcie-remote-exp)
#   IGVM_NAME=     staged vpci IGVM filename in WIN_WORKDIR (default openhcl-vfio-user.bin)
#   INSTANCE=      vfio-user device instance guid (default 11111111-2222-3333-4444-555555555555)
#   SOCK=          VTL2 unix socket path (default /tmp/vfio_nvme.sock)
#   BACKING_MB=    backing file size in MiB (default 256)
#   ADMIN_USER/ADMIN_PASS  guest creds (default Administrator / PcieRemote123!)
#   SKIP_REVIVE=1  skip the Lost/Live revive sub-test
#
# Prereq: provisioned guest.vhdx + a staged *vpci* OpenHCL IGVM in WIN_WORKDIR,
# ohcldiag-dev.exe + hyperv.psm1 in WIN_WORKDIR (one-time setup, see README).
set -uo pipefail

HERE="$(realpath "$(dirname "${BASH_SOURCE[0]}")")"
REPO="$(realpath "$HERE/../../../../..")"
WIN_WORKDIR="${WIN_WORKDIR:-/mnt/c/temp/pcie_remote_exp}"
VM_NAME="${VM_NAME:-pcie-remote-exp}"
IGVM_NAME="${IGVM_NAME:-openhcl-vfio-user.bin}"
INSTANCE="${INSTANCE:-11111111-2222-3333-4444-555555555555}"
SOCK="${SOCK:-/tmp/vfio_nvme.sock}"
BACKING_MB="${BACKING_MB:-256}"
ADMIN_USER="${ADMIN_USER:-Administrator}"
ADMIN_PASS="${ADMIN_PASS:-PcieRemote123!}"
MARKER="USNVMEMU-W6C-E2E-$(date +%Y%m%d%H%M%S)"
PS='/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe'
OHCL="$WIN_WORKDIR/ohcldiag-dev.exe"
MUSL="$REPO/usnvmemu/crates/nvme_firmware/target/x86_64-unknown-linux-musl/release/nvme_firmware"

fail() { echo "RESULT=FAIL $*"; exit 1; }
ohcl_run() { timeout "${1}" "$OHCL" "$VM_NAME" run -- sh -c "$2" 2>&1 | tr -d '\r' | sed 's/\x1b\[[0-9;]*m//g'; }

[ -x "$OHCL" ] || fail "ohcldiag-dev.exe not found at $OHCL"

if [ "${BUILD_FW:-0}" = "1" ]; then
    echo "== cross-build usnvmemu (nvme_firmware) static musl =="
    ( cd "$REPO/usnvmemu/crates/nvme_firmware" && \
      cargo build --bin nvme_firmware --no-default-features --features vfio-user \
        --target x86_64-unknown-linux-musl --release ) || fail "musl build failed"
fi
[ -x "$MUSL" ] || fail "usnvmemu musl binary missing: $MUSL (run with BUILD_FW=1)"

# --- 1. boot VM with the (vpci) IGVM + device cmdline (proven config, no wait-for-start) ---
# NB: OPENHCL_WAIT_FOR_START=1 reliably trips a Hyper-V VirtualizationException on
# this VM (finding, device-unrelated). We boot plain and bring usnvmemu Live early
# instead; with the W6c C-2 fix the VPCI offer latches DEV_00A9 at assemble anyway.
echo "== boot $VM_NAME (igvm=$IGVM_NAME instance=$INSTANCE) =="
BOOT=$("$PS" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "
  Import-Module '$(wslpath -w "$WIN_WORKDIR")\hyperv.psm1' -Force -EA Stop
  \$vm = Get-VM -Name '$VM_NAME' -EA Stop
  if (\$vm.State -ne 'Off') { Stop-VM '$VM_NAME' -TurnOff -Force; Start-Sleep 3 }
  Set-OpenHCLFirmware -Vm \$vm -IgvmFile '$(wslpath -w "$WIN_WORKDIR")\\$IGVM_NAME'
  Set-VmCommandLine -Vm \$vm -CommandLine 'OPENHCL_VFIO_USER_NVME=$INSTANCE:$SOCK'
  Start-VM '$VM_NAME'; Write-Host ('BOOTED ' + (Get-Date -Format o))
" 2>&1 | tr -d '\r')
# H-1: 严格以 BOOTED 出现为成功判据（"BOOTED" 只在 Start-VM 成功后才 Write-Host）。
# 不要用 grep -E "BOOTED|rror"——含 "Error" 的失败输出会误匹配而放行。先 echo 全部
# 输出供诊断，再严格 gate。$(...) 赋值已屏蔽 PS 退出码，避免 pipefail 误判。
echo "$BOOT" | sed 's/\x1b\[[0-9;]*m//g'
echo "$BOOT" | grep -q "BOOTED" || fail "boot failed (见上方输出)"

# --- 2. wait VTL2 reachable, push usnvmemu (base64 over `run -- sh -c`) ---
echo "== push usnvmemu into VTL2 =="
sleep 12
LSIZE=$(stat -c%s "$MUSL")
PUSHED=$(base64 -w0 < "$MUSL" | timeout 90 "$OHCL" "$VM_NAME" run -- sh -c \
  'base64 -d > /tmp/usnvmemu && chmod +x /tmp/usnvmemu && echo PUSHED rsize=$(stat -c%s /tmp/usnvmemu)' 2>&1 | tr -d '\r')
echo "$PUSHED (local=$LSIZE)"
echo "$PUSHED" | grep -qE "rsize=$LSIZE($|[^0-9])" || fail "usnvmemu push size mismatch"

# --- 3. create backing + launch usnvmemu detached (setsid) ---
echo "== create ${BACKING_MB}MiB backing + launch usnvmemu =="
ohcl_run 30 "truncate -s ${BACKING_MB}M /tmp/nvme_backing.img 2>/dev/null || dd if=/dev/zero of=/tmp/nvme_backing.img bs=1M count=${BACKING_MB} 2>/dev/null; setsid /tmp/usnvmemu --vfio-user-sock $SOCK --backing-file /tmp/nvme_backing.img < /dev/null > /tmp/usnvmemu.log 2>&1 & echo LAUNCHED"

# --- 4. wait for worker Live + confirm DMA_MAP issued (W6c) ---
echo "== wait worker Live + DMA_MAP =="
LIVE=0
for i in $(seq 1 12); do
    sleep 3
    if timeout 22 "$OHCL" "$VM_NAME" kmsg 2>/dev/null | tr -d '\r' | grep -q "reconnected, Live"; then LIVE=1; break; fi
done
[ "$LIVE" = "1" ] || fail "worker never went Live (check usnvmemu.log / vpci feature)"
DMA=$(ohcl_run 20 'grep -c "DMA_MAP added" /tmp/usnvmemu.log')
echo "worker Live; DMA_MAP added=$DMA"
# M-1: 判活锚定运行进程的 argv（含 --vfio-user-sock），而非裸 "usnvmemu"——后者会被
# 含 "usnvmemu" 的错误串（如 "sh: /tmp/usnvmemu: cannot execute"）误匹配成假 ALIVE。
ohcl_run 18 'ps aux 2>/dev/null | grep "[u]snvmemu" | head -1' | grep -q -- "--vfio-user-sock" || fail "usnvmemu not running (ps)"
[ "${DMA:-0}" -ge 1 ] || fail "0 DMA_MAP issued -> no zero-copy DMA (check underhill_mem sharing())"

# --- 5. guest oracle-1 (enumerate + re-init + 4 MiB IO) ---
echo "== guest oracle-1 (PSDirect) =="
VMID=$("$PS" -NoProfile -NonInteractive -Command "(Get-VM '$VM_NAME').Id.Guid" 2>&1 | tr -d '\r' | tr -d '[:space:]')
cp -f "$HERE/guest_io.ps1" "$WIN_WORKDIR/guest_io.ps1"
G=$("$PS" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$(wslpath -w "$WIN_WORKDIR")\\guest_io.ps1" \
    -VmId "$VMID" -Marker "$MARKER" -AdminUser "$ADMIN_USER" -AdminPass "$ADMIN_PASS" 2>&1 | tr -d '\r')
echo "$G"
echo "$G" | grep -q "ORACLE1=PASS" || fail "oracle-1 (guest readback) did not PASS"
echo "$G" | grep -q "DEV_00A9" || fail "guest did not enumerate DEV_00A9"

# --- 6. oracle-2: INDEPENDENT raw backing scan via STREAMING file -p (never VTL2 grep) ---
echo "== oracle-2 (independent raw backing scan via file -p) =="
HIT=$(timeout 180 "$OHCL" "$VM_NAME" file -p /tmp/nvme_backing.img 2>/dev/null | grep -a -bo "$MARKER" | head -1)
echo "marker @ $HIT"
echo "$HIT" | grep -q "$MARKER" || fail "oracle-2: marker NOT found in raw backing file"

# --- 7. revive sub-test: kill usnvmemu (by cmdline) -> Lost -> relaunch -> Live + DMA_MAP re-issued ---
if [ "${SKIP_REVIVE:-0}" != "1" ]; then
    echo "== revive: kill usnvmemu -> Lost -> relaunch -> Live =="
    ohcl_run 18 'pkill -f "/tmp/usnvmemu --vfio-user-sock"; sleep 1; echo killed'
    LOST=0
    for i in $(seq 1 6); do sleep 2; if timeout 20 "$OHCL" "$VM_NAME" kmsg 2>/dev/null | tr -d '\r' | grep -q "going Lost"; then LOST=1; break; fi; done
    echo "Lost detected=$LOST"
    ohcl_run 30 "setsid /tmp/usnvmemu --vfio-user-sock $SOCK --backing-file /tmp/nvme_backing.img < /dev/null >> /tmp/usnvmemu.log 2>&1 & echo RELAUNCHED"
    REVIVE=0
    for i in $(seq 1 10); do sleep 2; if [ "$(timeout 20 "$OHCL" "$VM_NAME" kmsg 2>/dev/null | tr -d '\r' | grep -c 'reconnected, Live')" -ge 2 ]; then REVIVE=1; break; fi; done
    echo "revive (2nd Live)=$REVIVE"
    [ "$REVIVE" = "1" ] || echo "WARN revive not observed (non-fatal)"
fi

echo ""
echo "RESULT=PASS marker=$MARKER dma_map=$DMA oracle2_offset=${HIT%%:*}"
echo "  oracle-1 (guest 4MiB readback) PASS + DEV_00A9 enumerated + NVMe disk online"
echo "  oracle-2 (independent raw backing scan) PASS @ byte ${HIT%%:*}"
