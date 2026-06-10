#!/bin/bash
# 仓库官方 cross-compile 方案的薄壳：从 WSL 编 Windows .exe。
# 不需要 xwin，复用本机已装的 VS Build Tools + Windows SDK。
#
# 用法（在 WSL 内仓库根目录）:
#   ./usnvmemu/scripts/build-windows-cross.sh <crate-name>
#   ./usnvmemu/scripts/build-windows-cross.sh ohcldiag-dev
#
# 前置 (WSL 端)：
#   sudo apt install clang-tools-20 llvm        # 提供 clang-cl-20 / llvm-lib-20
#   rustup target add x86_64-pc-windows-msvc
#   # 不需要装 lld：用 rustup 自带的 rust-lld，symlink 一下
#   mkdir -p ~/.local/bin
#   ln -sf $(rustup which rust-lld) ~/.local/bin/lld-link-20

set -e
CRATE="${1:-ohcldiag-dev}"
# 本脚本在 usnvmemu/scripts/（仓库根下 2 层）→ 仓库根 = dirname/../..。
# （2026-06-10 修：crate 2026-06-08 从 docs/superpowers/scripts/ 搬来后，旧
#  的 ../../.. 会解析到仓库根上一层，导致 `realpath build_support/windows_cross`
#  找不到——自改名以来此脚本一直没成功跑过。）
ROOT="$(realpath "$(dirname "${BASH_SOURCE[0]}")/../..")"
cd "$ROOT"

# 让 cross_tool.py 能找到 reg.exe / vswhere.exe / rust-lld
export PATH="$HOME/.local/bin:$PATH:/mnt/c/Windows/System32:/mnt/c/Program Files (x86)/Microsoft Visual Studio/Installer"

# 设置仓库要求的 cross 环境变量（绕过 setup_windows_cross.sh 的 wslpath bug）
TOOLDIR="$(realpath build_support/windows_cross)"
export CC_x86_64_pc_windows_msvc="$TOOLDIR/x86_64-clang-cl"
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="$TOOLDIR/x86_64-lld-link"
export AR_x86_64_pc_windows_msvc="$TOOLDIR/x86_64-llvm-lib"
export RC_x86_64_pc_windows_msvc="$TOOLDIR/x86_64-llvm-rc"
export DLLTOOL_x86_64_pc_windows_msvc="$TOOLDIR/x86_64-llvm-dlltool"
export MIDLRT_x86_64_pc_windows_msvc="$TOOLDIR/x86_64-midlrt.exe"
export OPENVMM_WINDOWS_CROSS_TOOL="$TOOLDIR/cross_tool.py"

# Refresh windows-cross cache (in case PATH changed since last run)
rm -f ~/.cache/windows-cross/cross-x86_64.json

# **Phase Q12** — 对 exclude 列表里的 standalone example，cd 到目录构建
# 而不 -p（否则 'package ID 不匹配'）。判断标准：crate 目录存在 +
# 根 Cargo.toml exclude 列表含此路径。
# 2026-06-08 修：crate 已搬到 usnvmemu/crates/ (原 docs/superpowers/examples/)。
EXAMPLE_DIR="usnvmemu/crates/$CRATE"
if [ -d "$EXAMPLE_DIR" ] && grep -q "\"$EXAMPLE_DIR\"" Cargo.toml; then
    echo "Building $CRATE (standalone example) for x86_64-pc-windows-msvc..."
    cd "$EXAMPLE_DIR"
    cargo build --target x86_64-pc-windows-msvc "${@:2}"
    echo
    echo "Output: $EXAMPLE_DIR/target/x86_64-pc-windows-msvc/debug/$CRATE.exe"
    ls -la "target/x86_64-pc-windows-msvc/debug/$CRATE.exe" 2>/dev/null || true
else
    echo "Building $CRATE for x86_64-pc-windows-msvc..."
    cargo build --target x86_64-pc-windows-msvc -p "$CRATE" "${@:2}"
    echo
    echo "Output: target/x86_64-pc-windows-msvc/debug/$CRATE.exe"
    ls -la "target/x86_64-pc-windows-msvc/debug/$CRATE.exe" 2>/dev/null || true
fi
