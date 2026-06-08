# tests/ — Integration test suite

> 31 个 integration test 文件 (2026-06-06 audit)；命名约定见
> [PRINCIPLES §10](../../../plans/PRINCIPLES.md)。

## 命名约定速查

| 前缀 | 含义 |
|------|------|
| `vt_<phase>_<topic>.rs` | 命名整齐版 (V-followup 段都用这个) |
| `<phase>_<topic>.rs` / `v<N>_<topic>.rs` | 早期 phase (V6/V7/V8) |
| `v_interop_<N>_<topic>.rs` | 真 Linux nvme-cli 互通发现的 wire 修 (V-interop 段) |
| `v_prp_list_<topic>.rs` | V-followup-prp-list 系列 |

## 按 phase 分组

**V5/V6 base** (admin / IO / AER):
- `bin_smoke.rs` (V5d binary 启动)
- `aer_e2e.rs` (V6c AER 完整 e2e)
- `discovery_e2e.rs` (V7 Discovery 完整 e2e)

**V8 multi-conn** (Arc<SharedControllerInner>):
- `v8b_multi_conn.rs` (多 conn 共享 controller)
- `v8c_per_conn_aer.rs` (per-conn AER routing)
- `v8d_disconnect.rs` (Disconnect 真清)
- `v8f_dual_listener.rs` (IO + Discovery 双 listener)

**V8e tokio refactor**:
- `v8e1_tokio_dep_smoke.rs` (tokio crate dep)
- `v8e2_bin_shutdown.rs` (#[tokio::main] + watch shutdown)
- `v8e3_async_session.rs` (AsyncSession + accept_and_handshake_async)
- `v8e4_aer_notify.rs` (Arc<Notify> AER wakeup < 10ms)
- `v8e5_kato.rs` (tokio::time::Sleep KATO timer)
- `v8e6_concurrent_e2e.rs` (多 conn 并发握手 e2e)
- `v8e7_2_async_fabric.rs` / `v8e7_3_async_dispatch.rs` (V8e-7 dispatch)
- `v8e7_followup_byte_identical.rs` (TLS-vs-plaintext app-layer 字节相等)

**V-interop (真 Linux nvme-cli wire 修)**:
- `v_interop_1_linux_nvme_tcp_sequence.rs` (Linux discover+connect sequence)
- `v_interop_2_fabric_io_connect.rs` (Fabric IO Connect)
- `v_interop_6_wire_capture.rs` (Get Log Page LPO + DiscoveryEntry layout)

**V-followup-prp-list (session chunking 16→256 LBA)**:
- `v_prp_list_chunking.rs` (5 anchor tests)

**V-followup-auth (host NQN allowlist + binding)**:
- `vt_auth_host_nqn_allowlist.rs`
- `vt_auth2_nqn_cert_binding.rs`

**V-followup-dhchap (DH-HMAC-CHAP)**:
- `vt_dhchap2_session_chap_state.rs` (state machine)
- `vt_dhchap3_bin_smoke.rs` (CLI flag + smoke)
- `vt_dhchap3w_auth_wire_e2e.rs` (simplified 2-msg wire)
- **`vt_dhchap4_spec_wire_e2e.rs`** (spec § 8.13.5 4-msg wire + 4d multi-descriptor; 10 e2e tests)

**V-followup-tls / mtls**:
- `vt_tls_1_stream_generic_smoke.rs` (AsyncSession<S> 泛型 stream)
- `vt_tls_3_handshake_e2e.rs` (TLS 1.3 handshake + NVMe-oF)
- `vt_tls_3_bin_smoke.rs` (bin TLS dual-listener)
- `vt_tls_4_app_layer_bytes_identical.rs` (TLS-vs-plaintext gate)
- `vt_mtls_handshake_e2e.rs` (强制 client cert)

## 跑全部

```bash
cd docs/superpowers/examples/nvme_of_tcp_target
cargo test                # 全部
cargo test --test vt_dhchap4_spec_wire_e2e  # 单文件
```

当前: 306 tests pass + clippy 0 warning。

## 加新 test 文件

1. 命名按"按 phase 分组"表，加 entry 到本 README。
2. 文件顶 `//! **Phase V-<...>** ...` doc comment 描述验什么。
3. 用 `#[tokio::test(flavor = "multi_thread")]` (V8e 之后默认)。
4. helper (handshake / Connect / build_*_pdu) 抄已有 test 文件，不复用 mod
   (不同 test file 是不同 binary)。
5. Anchor / regression test 名带 `_anchor_` / `_regression_` 便于 grep。
