# Phase V7 短 plan — Discovery Subsystem (Get Log Page 0x70)

> **✅ SHIPPED (2026-06-06 audit)** — V7a/b/c-fix 全段落地 (commits `3f3b79b1` → `dcfec8ed`)；105 tests reviewer-clean；CNTRLTYPE patch 修 reviewer H-1 BLOCK，Linux nvme-cli `nvme discover` 真生产可用。V-interop-5/6 又修了 DiscoveryEntry layout + LPO offset (见 [LESSONS.md](LESSONS.md) §1)。本文档保留为初版 Discovery 设计记录。

> **Status:** plan delivered (subagent ab6e80a3); 实现推下轮会话
> **Date:** 2026-06-06
> **Branch:** `feat/pcie-remote-experimental`
> **Prereq:** V6 完成 (commits c6e03aa6 / b9d3d5ec / d16f6ca0)，97 tests pass

## 0. 调研要点

| 事实 | 位置 | 影响 |
|---|---|---|
| controller admin.rs Get Log Page 已有 LID 0x01..0x0f / 0x80 / 0x81 分支，未知 LID 默认返 zeros | `controller/admin.rs:691-733` | 加 `0x70 => build_discovery_log(...)` 一行即可 hook |
| `controller/logs.rs` 无 `build_discovery_log` | `controller/logs.rs` | 必须新建 (spec § 5.16.1.20 Figure 350) |
| controller 已有 admin Connect / Property / Identify Controller 全栈 | V0..V3 | Discovery 复用，0 controller 行为差异 |
| `session.rs::handle_connect` 仅 log subnqn 不校验 | `session.rs:872-922` | V7 必加 NQN 校验 |
| bin `Cli` 无 `--mode` | `bin/nvme_of_tcp_target.rs` | 加单 flag `--discovery-mode` |

## 1. 实现策略

**选 A：V2Session 加 `discovery_mode: bool`**
- spec § 5.1.4 仅"command 子集"差异，不是独立 wire format
- session state machine / pdu / framing / digest 100% 共用
- admin_cmd 入口对白名单 opc 之外 SC=0x01 INVALID_OPCODE
- Connect 时校验 subnqn==Discovery NQN

## 2. 文件清单

**新增**：
- `nvme_firmware/src/controller/discovery_log.rs` — Figure 350 header + 1024B/entry × N (~140 LOC)
- `nvme_of_tcp_target/tests/discovery_e2e.rs` — e2e (~120 LOC)

**修改**：
- `controller/logs.rs` — re-export discovery_log
- `controller/admin.rs` Get Log Page match — `0x70 => build_discovery_log(...)`
- `controller/mod.rs` — 加 `discovery_nqn: String`、`discovery_portals: Vec<>`；新 `nvme_set_discovery_target` wrapper
- `nvme_of_tcp_target/src/session.rs` — handle_connect NQN 校验 + handle_admin_cmd 白名单
- `nvme_of_tcp_target/src/bin/nvme_of_tcp_target.rs` — `--discovery-mode` + `--discovery-target-nqn` + `--discovery-target-addr`
- `nvme_of_tcp_target/src/lib.rs` — `pub const DISCOVERY_NQN: &str = "nqn.2014-08.org.nvmexpress.discovery";`

## 3. 测试 (≥7)

**单测 (5)**：
1. `v7_discovery_log_header_byte_exact` — empty portals, GENCTR=0 / NUMREC=0 / RECFMT=0
2. `v7_discovery_log_two_entries_layout` — 2 portals, NUMREC=2, TRTYPE=3 (TCP) / ADRFAM=1 (IPv4) / SUBTYPE=2 (NVM)
3. `v7_discovery_log_truncates_to_requested_bytes` — host NumDW 限制
4. `v7_connect_rejects_non_discovery_nqn_in_discovery_mode` — Connect 非 DISCOVERY_NQN → SC=0x83
5. `v7_admin_io_opc_rejected_in_discovery_mode` — Write opc=0x01 → INVALID_OPCODE

**e2e (2)**：
1. `v7_e2e_discover_one_portal` — Discovery bin → Connect → Get Log 0x70 → 解 entry
2. `v7_e2e_discover_then_normal_target_on_separate_port` — 两 bin (discovery on 4420 + target on 4421)

## 4. 风险

| 标号 | 描述 | 处理 |
|---|---|---|
| R-1 | Discovery NQN 字符串错 → silent timeout | lib.rs const + e2e byte-exact |
| R-2 | Identify Ctrl CNTRLTYPE 没改 0x02 | nvme_set_discovery_target 内 patch |
| R-3 | inner-NUL NQN truncation 攻击 | subnqn_str 拒 inner-NUL |
| R-4 | Discovery Ctrl spec 限制 admin 子集 | 白名单 {Identify, Get Log Page, Keep Alive, Fabric, AER} |
| R-5 | --discovery-target-addr 不可达 → 客户端无尽重试 | bin 启动 probe 警告 |
| R-6 | NUMREC u64 LE byte ordering | zerocopy::U64<LE> + byte-exact test |
| R-7 | Discovery Log > 8 KiB (controller PRP1+PRP2 上限) | 教学 ≤4 portals；V-followup PRP list |
| R-8 | byte offset 脑补错 | log_test golden lock 前 32B |
| R-9 | discovery + target 共享 controller AER 路由错 | separate process；V8 才合并 |

## 5. bin 决策

**单 bin + flag 双行为**（拒绝双端口/双 bin）：

```bash
# Discovery target on 4420
nvme_of_tcp_target --listen 127.0.0.1:4420 --discovery-mode \
    --discovery-target-nqn nqn.2026-06.io.openhcl:nvme.userspace \
    --discovery-target-addr 127.0.0.1:4421

# Real NVM target on 4421
nvme_of_tcp_target --listen 127.0.0.1:4421
```

## 6. 不在 V7 范围

- TP4126 Centralized Discovery Pull Registration
- Discovery Log Page > 8 KiB (PRP list / SGL chain)
- AER Discovery Log Change (type=Notice info=0xF0)
- TLS 1.3 / DH-HMAC-CHAP
- Multi-target registry
- Disconnect 真清理（V8）
- Connect 主动 proxy

## 7. Definition of Done

- V7 单 commit (或拆 V7a logs hook + V7b session/bin/e2e 两段) + reviewer 0 critical/high
- 97 测全 green + 新 ≥7 测全 green
- clippy + fmt clean
- README 加 `### V7 Discovery` 章节 + Linux nvme-cli `discover` manual interop 步骤

## 8. V8 entrypoint hint

V8 时合并 V7 + V5-P4 多 conn 共享 controller + Disconnect 真清 + Arc-Mutex + tokio refactor + single-bin dual-listener。这是真正的大手术。

---

## 设计 Q&A

**Q: 为什么不让 controller 自带 discovery_mode bool？**
controller = PCIe/NVMe-NVM 语义；NVMe-oF Discovery = fabric 概念。controller 持
`discovery_nqn: String` + `portals: Vec<>` 纯数据；session 持 `discovery_mode: bool`
做 wire 行为差异 — 职责分离更干净。CNTRLTYPE 例外（NVM-level 字段）由 controller 持。

**Q: 为什么 R-7 不直接做 PRP list？**
教学 V7 80% 流程 ≤ 3 KiB；PRP list 是 controller-wide 改动，ROI 在 V8 / V-followup。

**Q: V7 与 V6 AER 冲突？**
不冲突。Discovery Ctrl 也支持 AER (spec § 5.1.4)。V6 infra 在 discovery_mode 下零改动可用。
