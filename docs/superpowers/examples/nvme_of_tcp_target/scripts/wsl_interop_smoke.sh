#!/usr/bin/env bash
# **V-followup-interop-1** — WSL2 内一键 nvme-cli loopback interop
#
# 跑法（需 sudo，因为 nvme connect 要 modprobe + /dev/nvmeX 节点）:
#   sudo bash docs/superpowers/examples/nvme_of_tcp_target/scripts/wsl_interop_smoke.sh
#
# 前置条件 (用户已手动满足):
#   sudo apt install -y nvme-cli
#   sudo modprobe nvme-tcp
#
# 本脚本假设 target bin 已在 127.0.0.1:4420 上跑 (本会话已 spawn)。
# 把所有 nvme-cli 输出落到 /tmp/wsl_interop_*.log 供 Claude 读后分析。

set -euo pipefail

LOG_DIR=/tmp
HOST_NQN=nqn.2014-08.org.nvmexpress:uuid:host-01
SUBSYS_NQN=nqn.2014-08.org.nvmexpress:teaching:disk
TARGET_IP=127.0.0.1
TARGET_PORT=4420

echo "=== STEP 1: nvme discover (无需 connect) ==="
nvme discover -t tcp -a "$TARGET_IP" -s "$TARGET_PORT" \
    --hostnqn "$HOST_NQN" 2>&1 | tee "$LOG_DIR/wsl_interop_discover.log" || true

echo
echo "=== STEP 2: nvme connect ==="
nvme connect -t tcp -a "$TARGET_IP" -s "$TARGET_PORT" \
    -n "$SUBSYS_NQN" --hostnqn "$HOST_NQN" \
    --keep-alive-tmo 10 \
    2>&1 | tee "$LOG_DIR/wsl_interop_connect.log" || true

echo
echo "=== STEP 3: lsblk 看是否出 /dev/nvmeX ==="
lsblk 2>&1 | tee "$LOG_DIR/wsl_interop_lsblk.log" | grep -E "(NAME|nvme)" || true

echo
echo "=== STEP 4: nvme list ==="
nvme list 2>&1 | tee "$LOG_DIR/wsl_interop_list.log" || true

DEV=$(nvme list 2>/dev/null | awk '/\/dev\/nvme/{print $1; exit}' || true)
if [ -n "${DEV:-}" ]; then
    echo
    echo "=== STEP 5: nvme id-ctrl $DEV (Identify Controller) ==="
    nvme id-ctrl "$DEV" 2>&1 | tee "$LOG_DIR/wsl_interop_id_ctrl.log" | head -80 || true

    echo
    echo "=== STEP 6: nvme id-ns ${DEV}n1 ==="
    nvme id-ns "${DEV}n1" 2>&1 | tee "$LOG_DIR/wsl_interop_id_ns.log" | head -40 || true

    echo
    echo "=== STEP 7: dd 4k Read ==="
    sudo dd if="${DEV}n1" of=/dev/null bs=4096 count=1 iflag=direct 2>&1 \
        | tee "$LOG_DIR/wsl_interop_dd_read.log" || true

    echo
    echo "=== STEP 8: dd 4k Write (会改 backing file，仅测试) ==="
    printf 'V-followup-interop-1 hello from nvme-cli\n' | sudo dd of="${DEV}n1" \
        bs=4096 count=1 oflag=direct 2>&1 \
        | tee "$LOG_DIR/wsl_interop_dd_write.log" || true

    echo
    echo "=== STEP 9: nvme disconnect ==="
    nvme disconnect -n "$SUBSYS_NQN" 2>&1 \
        | tee "$LOG_DIR/wsl_interop_disconnect.log" || true
else
    echo "未拿到 /dev/nvmeX 设备，nvme connect 应已失败"
fi

echo
echo "=== 全部日志 ==="
ls -la "$LOG_DIR"/wsl_interop_*.log
