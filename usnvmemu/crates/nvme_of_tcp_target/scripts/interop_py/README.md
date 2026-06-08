# scripts/interop_py — Python 跨进程 wire 实证 harness

> **uv-managed venv，stdlib only，0 sudo / 0 nvme-cli 依赖**。详 [DECISIONS](/usnvmemu/docs/DECISIONS.md) ADR-004。

每个 script 是 1 个独立 Python harness，验某个 wire / 行为。**模式**：起 Rust target → uv run python <script>.py → 退出码 0 = 全 scenario 通过。

## 启动

```bash
cd scripts/interop_py
uv sync                   # 首次：解锁 venv (pyproject.toml + uv.lock 全 stdlib only，没有真依赖)
# 另开 terminal 启动 Rust target，然后:
uv run python <script>.py
```

## Script 一览 (2026-06-06)

| Script | 验证 phase | 跑前置 (target CLI flag) |
|--------|-----------|------------------------|
| `tls_smoke.py` | V-followup-tls (TLS 1.3 handshake 基本通) | `--tls-listen ... --tls-cert ... --tls-key ... --tls-i-trust-this-cert` |
| `tls_e2e.py` | V-followup-tls (TLS + NVMe-oF 完整 IO) | 同上 |
| `mtls_smoke.py` | V-followup-mtls (强制 client cert) | 加 `--tls-client-ca <bundle>` |
| `nqn_cert_binding.py` | V-followup-auth-2 (NQN ↔ cert SAN/CN 绑定) | 加 `--tls-bind-nqn-to-cert` |
| `chap_e2e.py` | V-followup-dhchap-3-wire (simplified 2-msg wire) | `--host-secret <nqn>=<hex>` |
| `chap4_spec_wire_e2e.py` | **V-followup-dhchap-4 + 4d** (spec § 8.13.5 4-msg wire + multi-descriptor) | `--host-secret <nqn>=<hex>` |
| `io_write_e2e.py` | V5c (R2T + H2CData write 闭环) | 默认 plaintext |
| `io_size_sweep.py` | V-followup-prp-list (1..256 LBA byte-equal, 257 reject) | 默认 plaintext |
| `load_test.py` | V-interop-7 (多 conn 并发 IO) | 默认 plaintext |

## 加新 script

1. **命名** = wire / 行为，不是实现。例：`chap4_spec_wire_e2e.py` 不写
   `python_chap_test.py`。
2. **结构** 抄 `chap4_spec_wire_e2e.py` 头：`HOST / PORT / HOSTNQN /
   SUBNQN` 常量 + `fail() / ok() / recv_exact() / read_pdu() / write_pdu()`
   helper + 多 `scenario_*()` 函数 + `main()` dispatcher。
3. **首跑前修 PORT**：默认 4420，如目标用别的 port (`5555` 之类) 改一次跑完
   改回。
4. **失败时**：`fail("具体说什么不对")` + sys.exit(1)，让 CI 直接红。
5. **加 entry 到本表**。
6. **与 lib test 关系**: lib test 是单进程 in-process；Python harness 是跨
   进程；两者互补，缺一不可（见 [LESSONS §7](/usnvmemu/docs/LESSONS.md)）。

## 教学/生产边界

- **不验 Linux kernel `nvme-tcp.ko` 实际 wire**: 我们用自家 compute_response /
  自家 wire 构造，验自家算法对自家算法。真 kernel interop 留 ROADMAP §1
  HIGH (`real-host CHAP interop` + `kernel-CI 五元组`)。
- **不带 sudo**: 不调 `nvme connect`，因 nvme-tcp.ko 要 root + 模块加载。教学
  + CI-friendly 优先于"真 kernel"。
- **stdlib only**: 任何 `pip install` 类依赖一律不收（即使 uv 装得起来）；
  防止 reproducibility 被外部 PyPI 包污染。pyproject.toml 是空依赖 + Python
  3.10+ ；uv.lock 锁住 (没解析任何 dep)。
