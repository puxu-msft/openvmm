#!/usr/bin/env bash
# libvfio-user **differential oracle**（ADR-013 vfio 行 Tier B opt-in，真第三方独立实现）。
#
# 为什么是它：interop_py 的 Python harness 与我们 server 同源（同作者第二实现），
# vfio_user_wire_e2e（手搓 Rust client）也是同作者，vfio_user_guest_replay（真 kernel
# stimulus）权威但 golden 是 server 自身响应被冻。**唯一的真·第三方独立实现** = Nutanix
# 官方 C 库 [libvfio-user](https://github.com/nutanix/libvfio-user) 的 `samples/client`
# ——它用完全独立的 C 代码 parse/produce vfio-user wire（LESSONS §2）。历史上 catch 过 2 个
# 真 conformance bug（commit 533432ec：bogus-region 应 EINVAL / bulk REGION_READ）。
#
# **为何 opt-in / 非 standing**：非 hermetic（需 GitHub clone + meson/ninja build + libjson-c/
# cmocka dev）→ 按 ADR-013 走 Tier B 专属 opt-in job，不入 always-on gate。
#
# **差分信号（关键设计，见 POC + 对 pinned client.c 源核实的真实顺序）**：libvfio-user 的
# `samples/client` 是为 libvfio-user **自带的 sample server** 写的（device vid=0xdead/did=0xbeef +
# gpio/time/DMA/migration 语义），**不能**对我们的 NVMe server 端到端跑（设备语义不同）。但 abort
# **之前**它已用独立 C 实现成功走完一段协议前缀。**真实 `main()` 顺序（client.c @ pin）**：
#   VERSION 协商 → **bogus-region(0xdeadbeef) read → EINVAL（最前，紧接 VERSION）** →
#   GET_DEVICE_INFO（**硬断言 num_regions==9**，否则 errx 早退）→ GET_REGION_INFO 全循环 →
#   config-space 读 → **`assert(config_space.id.vid == 0xdead)`（sample 专属，我们 0x1414 → abort）**。
# 注：GET_IRQ_INFO / SET_IRQS / DMA / REGION_WRITE 都在 vid 断言**之后**，**本 gate 不覆盖**
#    （它们由 vfio_user_wire_e2e 手搓 oracle / 真 guest harness 覆盖）。
# 故：
#   PASS = client 输出含 ① bogus-region EINVAL（我们正确拒非法 region）+ ② 到达 vid 断言
#          （证 VERSION + bogus-EINVAL + GET_DEVICE_INFO + GET_REGION_INFO 循环 + config 读
#           这段协议前缀框架被独立 C 实现验通）。
#   FAIL = 任一标记缺失（协议回归会让 client 更早 abort / framing error / 挂）。
# **本 gate 继承的 pinned 契约（refresh 时必复核）**：client 硬依赖 `num_regions==9` +
#   config IDs `0xdead/0xbeef/0xcafe/0xbabe` + bogus-region 先于 vid 断言；任一上游变动会改"哪个
#   标记先 fire"。pin libvfio-user commit 保证这些 + abort 点/消息稳定。
#
# 用法：bash run_libvfio_differential.sh   （退出 0=差分通过）
# 环境：NVME_BIN（默认 ../../../nvme_firmware/target/debug/nvme_firmware）/ LIBVFIO_CACHE
set -uo pipefail

# ── pin（refresh checklist：bump 此 hash 时**逐条复核** —— ① abort 仍止于 vid 断言；
#    ② client 仍硬依赖 num_regions==9；③ config IDs 仍 0xdead/0xbeef/0xcafe/0xbabe；
#    ④ bogus-region read 仍先于 vid 断言。任一变 → 下方两标记的 grep 须同步改。owner=改本脚本者）──
PINNED_COMMIT="f633a2cb28bc8f388d36530eada43c902419cfbf"  # nutanix/libvfio-user, 2026-06 验
REPO="https://github.com/nutanix/libvfio-user.git"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NVME_BIN="${NVME_BIN:-$HERE/../../../nvme_firmware/target/debug/nvme_firmware}"
CACHE="${LIBVFIO_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/usnvmemu_libvfio}"
CLIENT="$CACHE/build/samples/client"

die() { echo "=== FAIL: $* ===" >&2; exit 1; }

# ── 1) 工具 + dev 头检查（缺则提示，不静默假通过）──
for t in git meson ninja cc pkg-config; do
    command -v "$t" >/dev/null || die "缺构建工具 $t（apt install meson ninja-build build-essential pkg-config）"
done
[ -f /usr/include/json-c/json.h ] || die "缺 libjson-c-dev（apt install libjson-c-dev）"
[ -f /usr/include/cmocka.h ] || die "缺 libcmocka-dev（apt install libcmocka-dev）"
[ -x "$NVME_BIN" ] || die "找不到 nvme_firmware bin：$NVME_BIN（先 cargo build --features vfio-user）"

# ── 2) clone + build pinned libvfio-user（缓存）──
if [ ! -x "$CLIENT" ] || [ "$(cat "$CACHE/.pinned" 2>/dev/null)" != "$PINNED_COMMIT" ]; then
    echo "=== build libvfio-user @ $PINNED_COMMIT ==="
    rm -rf "$CACHE"; mkdir -p "$CACHE"
    git clone --quiet "$REPO" "$CACHE" || die "clone libvfio-user 失败（网络?）"
    ( cd "$CACHE" && git checkout --quiet "$PINNED_COMMIT" ) || die "checkout pin 失败"
    ( cd "$CACHE" && meson setup build >/tmp/libvfio_meson.log 2>&1 && ninja -C build >/tmp/libvfio_ninja.log 2>&1 ) \
        || { tail -20 /tmp/libvfio_meson.log /tmp/libvfio_ninja.log; die "build libvfio-user 失败"; }
    echo "$PINNED_COMMIT" > "$CACHE/.pinned"
fi
[ -x "$CLIENT" ] || die "build 后仍无 samples/client"

# ── 3) spawn 我们的 server + 跑官方 client ──
TMP="$(mktemp -d /tmp/libvfio_diff.XXXXXX)"
trap 'kill "${SRV:-}" 2>/dev/null; wait "${SRV:-}" 2>/dev/null; rm -rf "$TMP"' EXIT
SOCK="$TMP/n.sock"; IMG="$TMP/n.img"; truncate -s 64M "$IMG"
RUST_LOG=warn "$NVME_BIN" --vfio-user-sock "$SOCK" --backing-file "$IMG" >"$TMP/srv.log" 2>&1 &
SRV=$!
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.05; done
[ -S "$SOCK" ] || { tail -20 "$TMP/srv.log"; die "server socket 未起"; }

echo "=== 跑官方 libvfio-user client（differential oracle）==="
timeout 30 "$CLIENT" "$SOCK" >"$TMP/client.log" 2>&1
CRC=$?
cat "$TMP/client.log"

# ── 4) 断言差分信号：bogus-region EINVAL + 到达 vid 断言 ──
# bogus region = 0xdeadbeef；client.c `warn("failed to %s region %d ...")` 用 **%d 有符号**渲染
# index → 0xdeadbeef 显示为 -559038737（唯一可能匹配值，故不列 unsigned/hex 死分支）。我们应回
# EINVAL/Invalid argument。
BOGUS_OK=0; grep -qE "failed to read from region -559038737 .*Invalid argument" "$TMP/client.log" && BOGUS_OK=1
# vid 断言 = client 走完整条协议前缀后才到的 sample-专属断言（我们 vid=0x1414≠0xdead）。
VID_REACHED=0; grep -qE "config_space\.id\.vid == 0xdead|client\.c:[0-9]+: main: Assertion .*vid" "$TMP/client.log" && VID_REACHED=1

echo "--- 差分判定 ---"
echo "bogus-region EINVAL（我们正确拒非法 region）: $BOGUS_OK"
echo "到达 vid 断言（整条协议前缀被独立 C 实现验通）: $VID_REACHED"
echo "client 退出码: $CRC（134/SIGABRT@vid 断言 = 预期；sample client 测 sample server 设备语义，不对 NVMe 端到端）"

if [ "$BOGUS_OK" = 1 ] && [ "$VID_REACHED" = 1 ]; then
    echo "=== PASS: libvfio-user 官方 C 实现确认我们的 vfio-user 协议前缀 wire-conformant ==="
    exit 0
fi
echo "--- server 日志尾 ---"; tail -10 "$TMP/srv.log"
die "协议前缀差分未通过（协议回归会让 client 更早 abort / framing error）：bogus=$BOGUS_OK vid=$VID_REACHED"
