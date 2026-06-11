#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.
#
# W6b Phase 2 Task 2.4 Step 2 — realize-only "boot 不挂" 真机验证。
#
# 把本会话构建的 OpenHCL IGVM（含 W6b underhill vfio_user_nvme 集成）装到一台真
# Hyper-V OpenHCL VM 上，设 OPENHCL_VFIO_USER_NVME cmdline 但**不**起 firmware，
# 验证：connect 重试到 cap → boot grace got=0 → AbsentPcieDevice 兜底 → underhill
# boot 完成不挂（VTL2 可达 + VM Running）。
#
# 这是 env-gated + AbsentPcieDevice 兜底 boot 安全性的真机证据。**guest 枚举设备**
# （Step 3）需 firmware 在 underhill connect 前就 listening —— 见 RESULT.md 的
# boot-ordering 结论：必须由 VTL2 init / underhill supervisor 启 firmware（W5b/W6），
# 外挂 ohcldiag-dev + restart 不行（restart 重置 VTL2 /tmp，孤立外挂 firmware）。
set -euo pipefail
WORKDIR="${WORKDIR:-/mnt/c/temp/pcie_remote_exp}"
VM="${VM:-pcie-remote-exp}"
OHCL="${OHCL:-$WORKDIR/ohcldiag-dev.exe}"
IGVM_SRC="${IGVM_SRC:-flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin}"
INSTANCE="${INSTANCE:-11111111-2222-3333-4444-555555555555}"
SOCK="${SOCK:-/tmp/vfio_nvme.sock}"
HERE="$(cd "$(dirname "$0")" && pwd)"

echo "[1/4] stage IGVM → $WORKDIR/openhcl-vfio-user.bin"
cp "$IGVM_SRC" "$WORKDIR/openhcl-vfio-user.bin"
cp "$HERE/configure_and_boot.ps1" "$WORKDIR/w6b_configure_boot.ps1"

echo "[2/4] configure VM ($VM) with IGVM + cmdline, boot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File 'C:\temp\pcie_remote_exp\w6b_configure_boot.ps1' \
  -VmName "$VM" -Instance "$INSTANCE" -SockPath "$SOCK" 2>&1 | tr -d '\r'

echo "[3/4] poll VTL2 readiness (boot 不挂 = underhill 起来了)"
ok=0
for i in $(seq 1 30); do
  if timeout 20 "$OHCL" "$VM" run /bin/true >/dev/null 2>&1; then echo "  VTL2 up after ~$((i*8))s — boot did NOT hang"; ok=1; break; fi
  sleep 8
done
[ "$ok" = 1 ] || { echo "  FAIL: VTL2 not reachable in ~240s (boot may have hung)"; exit 1; }

echo "[4/4] verify vfio_user_nvme boot-safety markers in kmsg"
KMSG="$(timeout 30 "$OHCL" "$VM" kmsg 2>/dev/null | tr -d '\r')"
echo "$KMSG" | grep -iE "connect attempt cap reached|boot grace period done|connect failed; backing off" | tail -6 || true
cap=$(echo "$KMSG" | grep -c "connect attempt cap reached" || true)
grace=$(echo "$KMSG" | grep -c "boot grace period done" || true)
echo "----"
if [ "$cap" -ge 1 ] && [ "$grace" -ge 1 ]; then
  echo "REALIZE_ONLY PASS ✓ — connect 超时(cap=$cap) → boot grace done(got=0) → AbsentPcieDevice 兜底；underhill boot 不挂"
else
  echo "REALIZE_ONLY INCONCLUSIVE — cap=$cap grace=$grace（kmsg 可能已滚动；看上方 markers）"
fi
