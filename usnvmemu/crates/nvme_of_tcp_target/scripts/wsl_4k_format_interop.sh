#!/usr/bin/env bash
# wsl_4k_format_interop.sh — 纯-4K Format + IO 真 nvme-cli 互通验证 (host-root)
#
# 验本会话新代码：`--allow-format` 的 Format NVM → LBAF[2] 纯-4K + 扇区感知
# `slba*4096` IO 偏移。**本内核可做，无需重编**（plaintext，不碰 CONFIG_NVME_AUTH）。
#
# ★ 关键 oracle（STEP 6）：写非零 start-block 后**直接读 backing file**，确认数据落在
#   `slba*4096` 而非 `slba*512`。纯 round-trip cmp（STEP 5）测不出"读写都用错 ×512"的
#   自洽 bug——读写经同一错偏移会自洽通过。backing file 是 kernel 看不到的 ground truth，
#   是真正独立的第三方 oracle。详 /usnvmemu/docs/LESSONS.md「自洽数据测不出自洽 bug」。
#
# 跑法（需 sudo）：
#   1) 普通终端先起 target（务必带 --allow-format）：
#        cd usnvmemu/crates/nvme_of_tcp_target
#        truncate -s 64M /tmp/ns1.img
#        cargo run --bin nvme_of_tcp_target -- \
#          --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img --allow-format
#   2) 另一个终端：
#        sudo BACKING=/tmp/ns1.img bash scripts/wsl_4k_format_interop.sh
#   3) 把 /tmp/host4k_*.log 全部 + 本脚本末尾 PASS/FAIL 行贴回给 Claude。
#
# 退出码：0 = 全 assert 通过；1 = 某 assert 失败（详见 stderr + 日志）；2 = 前置不满足。

set -uo pipefail   # 注意：不开 -e —— 单步失败要继续收集证据再退

LOG=/tmp
HOST_NQN=${HOST_NQN:-nqn.2014-08.org.nvmexpress:uuid:host-01}
SUBSYS_NQN=${SUBSYS_NQN:-nqn.2014-08.org.nvmexpress:teaching:disk}
IP=${IP:-127.0.0.1}
PORT=${PORT:-4420}
BACKING=${BACKING:-/tmp/ns1.img}
FAILS=0

note() { printf '\n=== %s ===\n' "$*"; }
fail() { printf '!! ASSERT FAIL: %s\n' "$*" >&2; FAILS=$((FAILS + 1)); }
ok()   { printf '   OK: %s\n' "$*"; }

[ "$(id -u)" -eq 0 ] || { echo "须 sudo 运行（nvme connect 要 root + /dev 节点）"; exit 2; }

note "STEP 0: 前置检查"
command -v nvme >/dev/null || { echo "缺 nvme-cli：sudo apt install -y nvme-cli"; exit 2; }
command -v python3 >/dev/null || { echo "缺 python3（生成 distinct pattern 用）"; exit 2; }
lsmod | grep -q nvme_tcp || modprobe nvme-tcp || true
[ -f "$BACKING" ] || { echo "backing file $BACKING 不存在 —— target 起了吗？路径对吗（BACKING=...）？"; exit 2; }
ok "nvme-cli + python3 + nvme_tcp + backing=$BACKING 就绪"

note "STEP 1: connect (plaintext)"
nvme connect -t tcp -a "$IP" -s "$PORT" -n "$SUBSYS_NQN" --hostnqn "$HOST_NQN" \
    --keep-alive-tmo 10 2>&1 | tee "$LOG/host4k_connect.log" || true
sleep 1

# 按 subsysnqn 找 controller（不抓"第一个 nvme"，避免误伤宿主真盘）
CTRL=""
for c in /sys/class/nvme/nvme*; do
    [ -r "$c/subsysnqn" ] || continue
    if grep -qF "$SUBSYS_NQN" "$c/subsysnqn"; then CTRL=$(basename "$c"); break; fi
done
if [ -z "$CTRL" ]; then
    echo "未找到 subsysnqn=$SUBSYS_NQN 的 controller —— connect 失败" >&2
    nvme list 2>&1 | tee "$LOG/host4k_list_fail.log"
    echo "贴回 host4k_connect.log + 上面 list + 'sudo dmesg | tail -40'"
    exit 1
fi
NS=/dev/${CTRL}n1
ok "controller=$CTRL  namespace=$NS"

# 设备节点卫生（RUNBOOK 实测坑：旧 `echo > /dev/nvmeXn1` 重定向会留普通文件挡块设备）
if [ -e "$NS" ] && [ ! -b "$NS" ]; then
    fail "$NS 不是块设备（应为 brw-）—— 疑似旧垃圾文件，rm 后请断开重连"
    rm -f "$NS"
fi

note "STEP 2: id-ns BEFORE format（期望 in-use = 512B = LBAF0）"
nvme id-ns "$NS" -H 2>&1 | tee "$LOG/host4k_idns_before.log" | grep -iE 'LBA Format|in use|Data Size' || true

note "STEP 3: format → LBAF[2] 纯-4K（target 必须带 --allow-format 否则被 block）"
nvme format "$NS" --lbaf=2 --force 2>&1 | tee "$LOG/host4k_format.log" || true
sleep 1

note "STEP 4: id-ns AFTER format（期望 in-use = 4096）"
nvme id-ns "$NS" -H 2>&1 | tee "$LOG/host4k_idns_after.log" >/dev/null
grep -iE 'LBA Format.*in use|in use|Data Size' "$LOG/host4k_idns_after.log" || true
if grep -iE 'in use' "$LOG/host4k_idns_after.log" | grep -q '4096'; then
    ok "in-use LBA size = 4096（Format NVM → 纯-4K 生效）"
else
    fail "format 后 in-use 仍非 4096 —— Format 被拒？target 漏了 --allow-format？见 host4k_format.log"
fi

note "STEP 5: 4K round-trip — distinct-per-block pattern（非均匀，避免自洽假阴性）"
# 8 个 4K block，block i 全填字节 (0xA0+i)，互不相同 —— 均匀 pattern 会掩盖跨 block 偏移错
python3 - "$LOG/host4k_w.bin" <<'PY'
import sys
with open(sys.argv[1], 'wb') as f:
    for i in range(8):
        f.write(bytes([0xA0 + i]) * 4096)
PY
nvme write "$NS" --start-block=0 --block-count=7 --data-size=32768 --data="$LOG/host4k_w.bin" \
    2>&1 | tee "$LOG/host4k_write.log" || true
nvme read "$NS" --start-block=0 --block-count=7 --data-size=32768 --data="$LOG/host4k_r.bin" \
    2>&1 | tee "$LOG/host4k_read.log" || true
if cmp -s "$LOG/host4k_w.bin" "$LOG/host4k_r.bin"; then
    ok "round-trip 32KiB identical（8×4K distinct block 全对）"
else
    fail "round-trip mismatch —— 见 host4k_read.log"
    cmp "$LOG/host4k_w.bin" "$LOG/host4k_r.bin" 2>&1 | head -3 >&2
fi

note "STEP 6: ★ 关键独立 oracle — 非零 start-block 写, 直查 backing file 物理偏移"
# 防 stale-data 假阳性（reviewer MEDIUM）：backing file 跨 run 不重置时，byte 20480 在
# "×512-bug 这一 run"里不会被写 → 若上个正确 run 残留 0xc5 会让 B_4K 假 PASS。对策：先把
# "×512 假设下命中 byte 20480 的那个 LBA"（=40，因 40*512=20480）清零；STEP5 已在"正确
# 假设下"覆盖 LBA5（→byte 20480）。两假设下 byte 20480 都被本 run 重写过，残留不可能存活。
# （正确固件下 LBA40 → byte 163840，落在 64M 内，无害。）
python3 - "$LOG/host4k_zero.bin" <<'PY'
import sys
open(sys.argv[1], 'wb').write(b'\x00' * 4096)
PY
nvme write "$NS" --start-block=40 --block-count=0 --data-size=4096 --data="$LOG/host4k_zero.bin" \
    2>&1 | tee "$LOG/host4k_prezero.log" || true
# 写 1 个 4K block 到 start-block=5，全填 0xC5
python3 - "$LOG/host4k_sb5.bin" <<'PY'
import sys
open(sys.argv[1], 'wb').write(b'\xc5' * 4096)
PY
nvme write "$NS" --start-block=5 --block-count=0 --data-size=4096 --data="$LOG/host4k_sb5.bin" \
    2>&1 | tee "$LOG/host4k_write_sb5.log" || true
# Flush 强制 target 把 backing file 落盘（host sync 刷不到 target 用户态缓冲）
nvme flush "$NS" 2>&1 | tee "$LOG/host4k_flush.log" || true
sync; sleep 1
# 独立 oracle：直接读 backing file —— kernel 看不到的 ground truth
#   纯-4K 正确 → 数据落在 byte 5*4096 = 20480
#   若 firmware 退化用 ×512 → 会落在 byte 5*512 = 2560
B_4K=$(dd if="$BACKING" bs=1 skip=20480 count=1 2>/dev/null | od -An -tx1 | tr -d ' ')
B_512=$(dd if="$BACKING" bs=1 skip=2560 count=1 2>/dev/null | od -An -tx1 | tr -d ' ')
{
    echo "backing[20480 (=5*4096)] = 0x$B_4K   (期望 c5)"
    echo "backing[2560  (=5*512) ] = 0x$B_512  (期望 非 c5，正确时为 STEP5 的 LBA0=0xa0)"
} | tee "$LOG/host4k_offset_oracle.log"
if [ "$B_4K" = "c5" ]; then
    ok "数据落在 5*4096 —— 扇区感知偏移正确"
else
    fail "5*4096 处非 0xc5（实为 0x$B_4K）—— 4K 偏移错或 Flush 未落盘"
fi
if [ "$B_512" = "c5" ]; then
    fail "5*512 处出现 0xc5 —— firmware 退化用了 ×512 旧偏移!"
else
    ok "5*512 处无 0xc5 —— 未退化到 ×512"
fi

note "STEP 7: disconnect"
nvme disconnect -n "$SUBSYS_NQN" 2>&1 | tee "$LOG/host4k_disconnect.log" || true

note "结果"
ls -la "$LOG"/host4k_*.log 2>/dev/null || true
if [ "$FAILS" -eq 0 ]; then
    echo "✅ PASS — 纯-4K Format+IO 真 nvme-cli 互通全部断言通过（贴回 /tmp/host4k_*.log 给 Claude 存证）"
    exit 0
else
    echo "❌ FAIL（$FAILS 项）— 见上方 ASSERT FAIL + /tmp/host4k_*.log，全部贴回给 Claude 诊断"
    exit 1
fi
