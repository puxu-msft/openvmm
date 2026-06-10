#!/usr/bin/env bash
# ── nvme_firmware 测试质量持续门 (Wave 1-5 建立的连续机制) ──
#
# "覆盖率%是 proxy；真正要保证的是  执行到 × 独立 oracle 验对 × 改坏会红 "。
# 三轴分别由：llvm-cov(执行到) / 独立 oracle+proptest(验对,非自洽) /
# cargo-mutants+revert-verify(有牙) 守。详见 docs/TEST_QUALITY.md。
#
# 用法：scripts/test-quality.sh {check|anchors|mutants|coverage|all}
set -euo pipefail
cd "$(dirname "$0")/.."
MODE="${1:-check}"

run_check() {  # 快门：build + lib + e2e + clippy + fmt
  cargo test --lib --no-default-features
  cargo test --test openhcl_pcie_remote_e2e
  cargo clippy --all-targets --no-default-features -- -D warnings
  cargo fmt --check
}

run_anchors() {  # M1 锚定护栏：spec 常量 == nvme_spec（drift 即红）
  cargo test --lib --no-default-features -- \
    sc_constants_match_nvme_spec \
    opcode_feature_register_constants_match_nvme_spec
}

run_mutants() {  # M6 变异测试：数据完整性热点文件"有没有牙"（自动 revert-verify）
  # 基线：sgl.rs + pi.rs 应 0 missed（Wave 1+3 达成）。新增逻辑务必保持。
  command -v cargo-mutants >/dev/null || { echo "需 cargo install cargo-mutants"; exit 1; }
  cargo mutants --in-place -p nvme_firmware \
    --file src/sgl.rs --file src/pi.rs \
    -- --no-default-features --lib
}

run_coverage() {  # M6 覆盖率：lib region/line（注：e2e 跨进程不计入，见 doc）
  command -v cargo-llvm-cov >/dev/null || { echo "需 cargo install cargo-llvm-cov"; exit 1; }
  cargo llvm-cov --no-default-features --lib --summary-only
}

case "$MODE" in
  check)    run_check ;;
  anchors)  run_anchors ;;
  mutants)  run_mutants ;;
  coverage) run_coverage ;;
  all)      run_check; run_anchors; run_coverage; run_mutants ;;
  *) echo "usage: $0 {check|anchors|mutants|coverage|all}"; exit 1 ;;
esac
echo "✅ test-quality [$MODE] done"
