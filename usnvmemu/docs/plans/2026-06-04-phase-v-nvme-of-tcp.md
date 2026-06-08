# Phase V — NVMe-over-Fabrics TCP Target Backend

> **✅ SHIPPED — 已大幅超越本文档 (2026-06-06 audit)** — V1..V8 + V8e tokio + V-followup-tls/mtls/auth/dhchap-3/dhchap-4/prp-list 全段落地。原本"non-goals" (TLS 1.3 / DH-HMAC-CHAP / multi-portal Discovery) 全部已实现并 reviewer-clean。最新坐标见 [ROADMAP.md](ROADMAP.md) §0。本文档保留为初版 design vision 记录。

> **Status:** design draft
> **Date:** 2026-06-04
> **Prereq:** Phase T（`trait Transport`）
> **Goal:** 把同一份 controller 作为 NVMe-oF TCP target 暴露在 4420 端口；
> 任何 host（Linux ≥ 5.0、Windows Server 2025）`nvme connect -t tcp` 即可挂载

## 1. 目标 / 非目标

**Goals (V1..V8):** ICReq/Connect/Property Get-Set/Identify/SGL R2/Read/Write/AER/Discovery；
通过 `nvme connect/list/id-ctrl/id-ns/smart-log/dd/disconnect`；
强制完成 Phase R2 SGL Segment chain；0 CRITICAL/HIGH。

**Non-goals:** TLS 1.3、DH-HMAC-CHAP、RDMA、TP4126 Centralized Discovery、
in-flight save/restore — 全部 V-followup。

## 2. 架构

```
                     trait Transport （Phase T 抽出）
                             ▲
                ┌────────────┼─────────────────┐
        OpenhclVsockTransport  VfioUserTransport  NvmeOfTcpTransport
        （existing）            （Phase U）          （Phase V）
                ▲                  ▲                    ▲
       OpenHCL VTL2          QEMU vfio-user        nvme-cli (any OS)
                                                   port 4420
```

## 3. Crate 布局（V0 后）

```
docs/superpowers/examples/
├── pcie_device_sdk/
├── nvme_firmware/       （existing，refactor 后变薄）
├── vfio_user_transport/               （Phase U 加）
├── nvme_controller_core/             NEW V0 —— lib crate
│   src/{lib.rs, transport.rs, cmd.rs, sgl.rs, pi.rs, regs.rs, controller/}
└── nvme_of_tcp_target/               NEW V1 —— sibling example crate
    src/{main.rs, pdu.rs, digest.rs, framing.rs, property.rs,
         fabric.rs, session.rs, nqn.rs, discovery.rs,
         transport_impl.rs, tests/}
```

V0 把 `nvme_firmware/src/{controller,cmd,sgl,pi,regs}.rs` 上提到
`nvme_controller_core` lib；`nvme_firmware` 变 thin adapter；
`nvme_of_tcp_target` 也只挂 `nvme_controller_core`，0 重复代码。

## 4. NVMe TCP Transport Spec 1.0a 对照

### 4.1 PDU framing（§ 3.4）
8-byte common header：PDU-Type、FLAGS（HDGSTF/DDGSTF/PDA）、HLEN、PDO、PLEN。
HDGST/DDGST CRC32C（Castagnoli）。

### 4.2 实现的 PDU type

| Type | Hex | 方向 | Phase | 备注 |
|---|---|---|---|---|
| ICReq | 0x00 | H→C | V2 | Init Connection Request |
| ICResp | 0x01 | C→H | V2 | Init Connection Response |
| H2CTermReq | 0x02 | H→C | V1 | decode + log |
| C2HTermReq | 0x03 | C→H | V1 | 协议错时发 |
| CapsuleCmd | 0x04 | H→C | V2 | 64-B SQE + 可选 inline data |
| CapsuleResp | 0x05 | C→H | V2 | 16-B CQE |
| H2CData | 0x06 | H→C | V4 | R2T 后的 host data |
| C2HData | 0x07 | C→H | V4 | controller data (Read) |
| R2T | 0x09 | C→H | V4 | Ready-to-Transfer |

### 4.3 SGL 规则（§ 3.5）
PRP 非法。`SQE.PSDT` 必须 01/10。类型 0x0 (in-capsule)/0x5 (transport-specific=
跟 H2C/C2H Data PDU)/0x2/0x3 (Segment chain)。

### 4.4 Fabric Commands（NVMe Base 2.0c § 6，opcode 0x7F + FCType）

| FCType | Name | Phase | 映射 |
|---|---|---|---|
| 0x00 | Property Set | V2 | BAR0 写（CC/AQA/ASQ/ACQ） |
| 0x01 | Connect | V2 | NEW —— 建队列 + 绑 NQN |
| 0x04 | Property Get | V2 | BAR0 读（CAP/VS/CSTS） |
| 0x05/06 | Auth Send/Receive | V-followup | DH-HMAC-CHAP |
| 0x08 | Disconnect | V8 | 拆队列 |

## 5. Property ↔ BAR0 shim

| Property offset | BAR0 reg | Dir |
|---|---|---|
| 0x00 (8B) | CAP | RO |
| 0x08 (4B) | VS | RO |
| 0x14 (4B) | CC | RW |
| 0x1c (4B) | CSTS | RO |
| 0x20 (4B) | NSSR | WO (no-op) |
| 0x24..0x30 | Connect cmd 替代 | — |

V2 只调 `controller.mmio_read(bar=0, offset, size)` / `mmio_write(...)`。Controller 0 改动。

## 6. NVMe-oF 概念 ↔ 既有实现

| NVMe-oF 概念 | 既有 artifact | 新工作 |
|---|---|---|
| ICReq/ICResp | 无 | V1 framing + V2 negotiation |
| Property Get/Set | `controller/mmio.rs` | V2 shim |
| Connect (qid=0) | `controller/enable.rs` admin init | V2 thin wrapper |
| Connect (qid≥1) | `admin.rs` Create IO SQ/CQ 0x01/0x05 | V2 synthesize + dispatch |
| SQE Fabric opcode 0x7F | 无 | V2 |
| SQE Admin/NVM | `admin.rs` `io.rs` | 复用 |
| SGL Data Block in-capsule | `sgl.rs` R1 | 复用 |
| SGL Segment chain | `sgl.rs` (TODO R2) | **V4 强制实现** |
| SGL transport-specific 0x5 | 无 | V4 —— 驱动 R2T 循环 |
| R2T | `transport.read_data()` token | V4 wire encoding |
| H2CData | `read_data()` completion | V4 wire decode + 重组 |
| C2HData | `transport.write_data()` | V4 wire encoding |
| CqeResp | `transport.write_cq_entry()` | V2 wire encoding |
| Interrupt | `transport.fire_interrupt()` | V2 no-op（TCP host 轮询） |
| AER 0x0c | `admin.rs` AER queue + tick | V6 ensure CqeResp 唤醒 host |
| Discovery log 0x70 | `logs.rs` 占位 | V7 填 NQN + portal |
| Identify CNS 0x00..0x06 | `admin.rs` | 复用 |
| HOSTID 16-byte | Phase J `controller/reservation.rs` | 复用（Connect 带 HOSTID） |

## 7. 阶段化实施（每 = 1 commit + 1 reviewer 轮）

| Phase | 关注 | Impl LOC | Test LOC |
|---|---|---:|---:|
| **V0** | 抽 `nvme_controller_core` lib；`pub(super)` → `pub(crate)`；67 测试不变 | 50 (moves) | 0 |
| **V1** | `pdu.rs` (zerocopy)、`digest.rs` (CRC32C Castagnoli)、`framing.rs` (tokio AsyncRead)、TcpListener 骨架 | 350 | 100 |
| **V2** | `property.rs`、`fabric.rs` (Connect)、`session.rs` state machine、minimal `impl Transport` | 500 | 200 |
| **V3** | Identify + Get Log Page via in-capsule C2HData ≤ 4 KiB；Discovery NQN stub | 150 | 100 |
| **V4** | SGL Segment chain walking + R2T/H2CData 循环 + MAXH2CDATA 分片 | 450 | 150 |
| **V5** | IO Read/Write 与 nvme-cli 互操；自写 Rust nvme-tcp client in tests/ | 50 | 450 |
| **V6** | AER 投递（controller `tick()` AER → CapsuleResp 不需 host prompt） | 100 | 50 |
| **V7** | Discovery subsystem（Get Log Page 0x70，NVMe Base § 5.16.1.20） | 200 | 50 |
| **V8** | Multi-queue + Disconnect（`Arc<tokio::sync::Mutex<Core>>`） | 250 | 100 |
| **Total** | | **2100** | **1700** |

## 8. Transport 翻译

per-session tokio task 持 `NvmeOfTcpSession { socket, controller, in_flight:
HashMap<u64, PendingDma>, pending_r2t, hdr_digest, data_digest }`：

- `transport.read_data(desc, len)` → emit R2T PDU + 存 `PendingDma::ReadHostData` →
  sync 返 token；H2CData 到达匹配 TTAG 后 → 组装 bytes → 调 `controller.on_dma_complete(token, ok, data)`
- `transport.write_data` → emit C2HData + 立即 `on_dma_complete(token, true, vec![])`
- `transport.fire_interrupt` → no-op（TCP host 内联轮询，无 MSI-X）

## 9. 测试

- **Unit per phase**：encode/decode roundtrip；CRC32C verify；Connect 重复 CNTLID 拒；
  Identify byte-exact vs `nvme_spec` golden；3-segment SGL chain walk；AER 不需 prior
  host cmd；Discovery log byte-exact（NVMe Base Figure 350）
- **Integration**（`tests/interop.rs`，`--features interop`）：~300 LOC 纯 Rust nvme-tcp
  client：ICReq → Connect → Property Set CC.EN → Identify → Write 8 KiB → Read 8 KiB
  → byte 对比 → Disconnect
- **Real-host manual**：Linux box `nvme discover/connect/list/dd/smart-log/disconnect`
- **Coverage**：80%+ via `cargo llvm-cov`

## 10. 开放问题

1. **NQN auto-gen**：deterministic `sha256(backing_files)` 重启可重复，or CLI-only？倾向 deterministic
2. **CNTLID**：单调从 1（日志友好）vs 随机 16-bit。倾向单调
3. **共享 backing files 跨 transport**（pcie_remote + nvme_of_tcp 同进程）：留 V8.5，需要 controller core Mutex
4. **TLS 1.3 + PSK**（Linux 6.7+）：V-followup，用 `rustls`。Transport 设计不阻塞
5. **DH-HMAC-CHAP**（FCType 0x05/06）：V-followup，~600 LOC
6. **MAXH2CDATA**：宣告 64 KiB（与 Linux target 默认对齐）
7. **ICDOFF**（Write inline data）：留 V4.5，省 1 RTT 但 SGL parser 复杂

## 11. 关键决策

- **Tokio vs pal_async**（V1+）：选 **tokio**（first-class TCP/BytesMut/select! 人体工学，
  独立 service 零耦合 OpenVMM）。合并 binary 后跑两 runtime 共享 controller 经 `Arc<Mutex<…>>`
- **`forbid(unsafe_code)`**：zerocopy 给 byte-exact 解码无需 unsafe
- **TCP 下 SGL fetch**：无 RDMA Read 等价；host 把全部 SGL inline 在 CapsuleCmd
  或经 dedicated H2CData。V4 inline 优先，V4b H2CData-bearing SGL
- **Mutex 下 re-entry**：用 `tokio::sync::Mutex`（async-aware，无同任务死锁）；
  `nvme_controller_core/README.md` 显著文档化约束

## 12. Acceptance（V8 done）

1. 从 LAN 另一台 Linux box 跑 `nvme discover/connect/list/dd/smart-log/disconnect` 全绿
2. `cargo test --workspace --all-features` 绿；`cargo clippy --all-targets -- -D warnings` clean
3. `rust-reviewer` 累计 V1..V8 0 CRITICAL / 0 HIGH
4. README 含 interop 步骤；现有 67 controller_core 测试 + pcie_remote 不回归

## 13. V-followup

DH-HMAC-CHAP / TLS 1.3 + PSK / Centralized Discovery (TP4126) / 持久化 reservation
跨 reconnect / 多 PDU C2HData telemetry / combined-transport binary
（`--enable-both-transports`）

## 附录 A：参考

- NVMe TCP Transport Spec 1.0a
- NVMe Base 2.0c § 1.5.10 + § 6 + § 8.13
- Linux kernel `drivers/nvme/target/tcp.c`（读不抄）
- SPDK NVMe-oF TCP target（C 参考）
