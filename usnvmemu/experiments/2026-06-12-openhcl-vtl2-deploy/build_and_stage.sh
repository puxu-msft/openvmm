#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation. Licensed under the MIT License.
# W5a 档2：cross-build firmware + test client 静态 musl ELF + stage 到 /tmp 打包。
set -euo pipefail
REPO=$(cd "$(dirname "$0")/../../.." && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
STAGE="${STAGE:-/tmp/w5a_stage}"
mkdir -p "$STAGE"

echo "[build] firmware (static musl release)"
( cd "$REPO/usnvmemu/crates/nvme_firmware" && \
  cargo build --bin nvme_firmware --no-default-features --features vfio-user \
    --target x86_64-unknown-linux-musl --release )
cp "$REPO/usnvmemu/crates/nvme_firmware/target/x86_64-unknown-linux-musl/release/nvme_firmware" "$STAGE/fw"

echo "[build] test client (static musl release)"
( cd "$HERE/client" && cargo build --release --target x86_64-unknown-linux-musl )
cp "$HERE/client/target/x86_64-unknown-linux-musl/release/w5a_vtl2_client" "$STAGE/cl"

strip "$STAGE/fw" "$STAGE/cl" 2>/dev/null || true
ls -lh "$STAGE/fw" "$STAGE/cl"
echo "[build] staged at $STAGE (fw + cl)"
