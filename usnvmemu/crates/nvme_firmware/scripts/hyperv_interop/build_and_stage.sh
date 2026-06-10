#!/bin/bash
# WSL-side: cross-build the CURRENT nvme_firmware as a Windows .exe and stage it
# into the Windows working dir consumed by run_hyperv_nvme_e2e.ps1.
#
# This is deliberately thin — it just calls the repo's official cross-compile
# wrapper with the right feature set and copies the artifact. The OpenHCL/vsock
# transport is gated by the `openhcl` feature; `vfio-user` (default) is Unix-only,
# so the Windows build is `--no-default-features --features openhcl`.
#
# Usage (from anywhere in WSL):
#   usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/build_and_stage.sh [WIN_WORKDIR]
#     WIN_WORKDIR: WSL path to the Windows staging dir
#                  (default /mnt/c/temp/pcie_remote_exp)
#
# Prereqs (one-time, see usnvmemu/scripts/build-windows-cross.sh header):
#   rustup target add x86_64-pc-windows-msvc
#   sudo apt install clang-tools-20 llvm
#   ln -sf "$(rustup which rust-lld)" ~/.local/bin/lld-link-20
#   + VS Build Tools + Windows SDK installed on the Windows host
set -euo pipefail
WIN_WORKDIR="${1:-/mnt/c/temp/pcie_remote_exp}"
ROOT="$(realpath "$(dirname "${BASH_SOURCE[0]}")/../../../../..")"
cd "$ROOT"

echo "== cross-building nvme_firmware.exe (release, openhcl feature) from $ROOT =="
./usnvmemu/scripts/build-windows-cross.sh nvme_firmware --release --no-default-features --features openhcl

EXE="usnvmemu/crates/nvme_firmware/target/x86_64-pc-windows-msvc/release/nvme_firmware.exe"
if [ ! -f "$EXE" ]; then
    echo "ERROR: build did not produce $EXE" >&2
    exit 1
fi

mkdir -p "$WIN_WORKDIR"
cp -f "$EXE" "$WIN_WORKDIR/nvme_firmware.exe"
echo "== staged =="
ls -la "$WIN_WORKDIR/nvme_firmware.exe"
