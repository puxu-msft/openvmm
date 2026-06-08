#!/bin/bash
# Build OpenHCL IGVM with vpci feature + pcie_remote instance manifest.
#
# 用法（WSL 内仓库根）：
#   ./docs/superpowers/scripts/hyperv/build_pcie_igvm.sh                  # default 名字 pcie-latest
#   ./docs/superpowers/scripts/hyperv/build_pcie_igvm.sh my-tag           # 自定义后缀
#   ./docs/superpowers/scripts/hyperv/build_pcie_igvm.sh my-tag /tmp/m.json  # 自定义 manifest
#
# 输出：flowey-out/artifacts/build-igvm/ship/<tag>/openhcl-<tag>.bin
#
# 配套：复制此 .bin 到 Windows 然后 ./openhcl/Set-OpenHCL-HyperV-VM.ps1 加载。

set -e

TAG="${1:-pcie-latest}"
MANIFEST="${2:-$(dirname "${BASH_SOURCE[0]}")/openhcl-x64-pcie.json}"

ROOT="$(realpath "$(dirname "${BASH_SOURCE[0]}")/../../../..")"
cd "$ROOT"

if [ ! -f "$MANIFEST" ]; then
  echo "manifest not found: $MANIFEST" >&2
  exit 1
fi

echo "Building OpenHCL IGVM with tag=$TAG manifest=$MANIFEST"
cargo xflowey build-igvm x64 --release \
  --override-manifest "$MANIFEST" \
  --override-openvmm-hcl-feature vpci \
  -o "$TAG"

OUT="$ROOT/flowey-out/artifacts/build-igvm/ship/$TAG/openhcl-$TAG.bin"
if [ -f "$OUT" ]; then
  echo
  echo "IGVM ready: $OUT"
  echo "size: $(du -h "$OUT" | cut -f1)"
else
  echo "build succeeded but expected output missing: $OUT" >&2
  exit 1
fi
