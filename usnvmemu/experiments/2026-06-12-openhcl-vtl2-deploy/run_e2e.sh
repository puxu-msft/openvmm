#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.
# W5a 档2：把 firmware+client 打 tar→base64→单条 ohcldiag-dev run 推进 VTL2，
# 起 firmware（后台）+ 跑 client（前台 oracle）。
set -euo pipefail
STAGE="${STAGE:-/tmp/w5a_stage}"
VM="${VM:-pcie-remote-exp}"
OHCLDIAG="${OHCLDIAG:-/mnt/c/temp/pcie_remote_exp/ohcldiag-dev.exe}"
GPA_BASE="${W5A_GPA_BASE:-0x100000}"

[ -x "$STAGE/fw" ] && [ -x "$STAGE/cl" ] || { echo "missing $STAGE/fw|cl — run build_and_stage.sh first"; exit 1; }

# VTL2 侧脚本：解 tar → 造 backing file → 后台起 firmware → 等 socket → 跑 client → kill firmware。
# 注：NvmeController::open 要求 ≥1 个 --backing-file（即便 Identify Controller 不读 NS），
# 否则 firmware 起不来。set +e 包住 client 调用，保证失败也 dump fw.log。
VTL2_SCRIPT='
set -e
cd /tmp
base64 -d | tar x
chmod +x fw cl
truncate -s 1M /tmp/ns1.img 2>/dev/null || dd if=/dev/zero of=/tmp/ns1.img bs=1024 count=1024 2>/dev/null
./fw --vfio-user-sock /tmp/fw.sock --backing-file /tmp/ns1.img >/tmp/fw.log 2>&1 &
FWPID=$!
for i in $(seq 1 100); do [ -S /tmp/fw.sock ] && break; sleep 0.05; done
set +e
W5A_GPA_BASE='"$GPA_BASE"' ./cl /tmp/fw.sock
RC=$?
set -e
kill $FWPID 2>/dev/null || true
echo "=== fw.log ==="; cat /tmp/fw.log
exit $RC
'

# 打 tar（fw+cl）→ base64 → 经 ohcldiag-dev run 的 stdin 流进 VTL2。
tar c -C "$STAGE" fw cl | base64 -w0 | \
  "$OHCLDIAG" "$VM" run /bin/sh -- -c "$VTL2_SCRIPT" 2>&1 | tr -d '\r'
