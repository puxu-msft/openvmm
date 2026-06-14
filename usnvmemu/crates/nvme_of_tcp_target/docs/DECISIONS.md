# nvme_of_tcp_target — 决策日志 (crate-local ADR)

> 本 crate 自有的 Architecture Decision Records。跨切面 / 架构级决策见
> [项目 DECISIONS.md](/usnvmemu/docs/DECISIONS.md)（含全 ADR 索引表）。
> 时间倒序。

---

## ADR-008 — Windows IO Connect 走 transport-SGL（保 IOCCSZ=4），不退化加 in-capsule IO data (2026-06-14)

**Context**：真 WS2025 inbox initiator（`stornvmeofi`/`nvmeofutil`）真连暴露：IO-queue Fabric Connect
的 1024B Connect data 不放 in-capsule，而是用 **Transport SGL（`sqe[39]=0x5A` = type 0x5/subtype 0xA）
经 R2T/H2CData 推送**。根因是 Identify Controller `IOCCSZ=4`（[cmd.rs:769](/usnvmemu/crates/nvme_firmware/src/cmd.rs)）
广告「IO 命令胶囊 = 64B SQE，**不支持 in-capsule data**」——Windows 据此正确改用 transport SGL。但旧
`handle_connect` 在 parse 前 `if data.len()!=1024 { reject SC=0x82 }`，**从不发 Windows 等的 R2T** →
IO 队列建不起、namespace 不出盘。**内部不一致**：target 广告「IO 无 in-capsule」却要求 Connect data
in-capsule。Linux `nvme-cli` 总把 Connect data 放 in-capsule，故纯 Linux 永远盖不到这条岔路。

**Options**：
- A: **把 IOCCSZ 抬高**（广告 IO 支持 in-capsule data）→ Windows 就会把 Connect data 放 in-capsule，
  无需 R2T。但这是**对整个 firmware 的 wire 契约谎报**——controller 实际不消费 in-capsule IO write
  payload；且改 IOCCSZ 影响所有 transport 与所有 IO 命令，回归面大、违 spec 真相。
- B: **保 IOCCSZ=4，在 target 实现 transport-SGL Connect 路径**（发 R2T 取 Connect data）。只动
  nvme_of_tcp_target 的 Connect 解析，firmware 契约不变。

**Decision**：**B**。`fabric::decode_connect_data_source(sqe, in_capsule_len)` 纯函数分流
`InCapsule`/`ViaR2t{len}`/`Invalid`（sync/async 共享，防漂移）；`ViaR2t` 经 `dma_read_via_r2t` 发 R2T
取 1024B。IOCCSZ 维持 spec-consistent 的 4。连带两笔（同一真连暴露）：
- **Windows H2CData PLEN=HLEN quirk**：Windows 把 H2CData CommonHdr PLEN 设为 HLEN（**不计 data**，违
  NVMe/TCP transport binding（TP-8000）「PLEN 须含 data」），data 长由 PSH DATAL 给。target 据 DATAL 补读
  尾随 data（越界 DATAL 仍拒、不补读越界）；所有 R2T host-data 读加 30s inactivity timeout（host 半开/
  卡死兜底，DoS bound ≤64KiB）。
- **SCT 0x07→0x01 修复**（CRITICAL，architect 发现）：fabrics SC(0x80-0x9F) 原用 SCT=0x07 Vendor
  Specific 是既存 wire bug，应 SCT=0x01 Command Specific（IPO/IATTR 仅此下定义）。**独立外部 oracle 核实**：
  Linux `include/linux/nvme.h` `NVME_SC_CONNECT_INVALID_PARAM = 0x182`（= SCT `0x1`<<8 | SC `0x82`），
  `NVME_SC_CONNECT_FORMAT = 0x180`..`INVALID_HOST = 0x184` 全在 `0x18x` 区 ⇒ fabrics Connect 状态码归
  SCT=0x1 Command Specific，确证（非自家文档自证）。

**Consequences**：
- 真 WS2025 `Get-Disk` 出盘 Online/64 MiB + 4KB byte-equal IO（C3 commit `4df5c3f83`）。
- 新增 regression gate `tests/vt_windows_transport_sgl_connect.rs`（3 测：SGL 0x5A + H2CData PLEN=HLEN
  + 越界拒）锁定 Windows wire。
- **教训**：跨实现互通盲区藏在「另一端默认走的窄路」之外——见 [LESSONS §33](/usnvmemu/docs/LESSONS.md)。
- sync `V2Session.handle_connect` 也镜像了路由，但其 `await_host_data` **不含 PLEN quirk 补读**（仅对
  spec-conformant H2CData 正确；Windows 只走 AsyncSession，sync 是 legacy/test）——已在源码注明。

**Status**：active（C1+C2+C3 全 shipped，真机 PROVEN）。

---

## ADR-007 — TP-8011 PSK 不 fork rustls，等上游 (2026-06-06)

**Context**：实现 NVMe TP-8011 需 TLS 1.3 external PSK；rustls 0.23 没公开
API；SpiderOak fork (PR #2424) closed without merge。

**Options**：
- A: 等 rustls upstream — 时间不可控
- B: 自 fork rustls — 高维护成本，安全债
- C: SpiderOak fork 当 git dep — 教学 OK，prod 不可
- D: 换 TLS 栈 (s2n-tls / openssl) — 800-1500 LOC 重写

**Decision**：路径 A + C 并行（短期不动主线，等上游；C 作为可选实验脚本）。

**Consequences**：
- `src/tls_psk.rs` 留 deterministic crypto only，标 "rustls 注入待上游"
- 出 detailed survey 文档 [2026-06-06-phase-v-followup-tls-psk-survey.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)
- 优先级降为"等 + monitor"；HIGH 优先 swap 给 *kernel CI vector* (独立)

**Status**：active；rustls 出 external PSK API 后 revisit。

---

## ADR-006 — V-followup-prp-list 走 session-level chunking 不动 controller (2026-06-06)

**Context**：V5_NLB_MAX=16 LBA (8 KiB) 卡死 IO 大小；MDTS=5 (128 KiB) 名义
support 但实际 reject。

**Options**：
- A: 改 controller PRP-list path (扩 sentinel decode + multi-page)
  ~ 500 LOC + 跨 lib 改动 + 新 reviewer round
- B: session 端透明 chunking (16 → 256 LBA, 内部分 16 个 sub-cmd)
  ~ 200 LOC，0 controller 改动

**Decision**：B。

**Consequences**：
- 单 IO 上限提到 256 LBA / 128 KiB，符合 MDTS
- host 不感知 chunking；wire 仍走每 sub-cmd 一组 C2HData
- *Future production* 路径仍是 A，本文档保留作为起点
- plan 文档标 SUPERSEDED + 留 rationale

**Status**：active；real-Linux interop 实测过后可考虑回 A。

---

## ADR-005 — DHCHAP wire 同时支持 simplified + spec 4-msg (2026-06-06)

**Context**：V-followup-dhchap-3 已落地教学版 simplified wire (2-msg)，
Linux nvme-cli 实发 spec § 8.13.5 4-msg (NEGOTIATE/CHALLENGE/REPLY/SUCCESS1)。
新加 4-msg 是否破坏老 simplified？

**Options**：
- A: 删 simplified，只留 spec — 删 V-dhchap-3 测试 + 改 Python harness
- B: 双 wire 共存，host 首发包自动识别

**Decision**：B（`ChapWireMode::Unknown → Spec4Msg/Simplified` 三态锁定）。

**Consequences**：
- 兼容 V-dhchap-3 全部测试 (105 个) + V-dhchap-4 新增 10 e2e
- 自动检测的 collision 概率 ≈ 2^-32 (4 条 condition 同时满足，见 reviewer L-3)
- 文档承载额外复杂度 (两条路径)
- *设计模式*：wire 自动识别 + 锁定，是后向兼容的可复用模板 (见
  [LESSONS §11](/usnvmemu/docs/LESSONS.md) decision-then-IO 模式同段落)

**Status**：active；若未来 simplified 路径再没人用，可新开一条 deprecate ADR。

---

## ADR-004 — Python interop 不走 sudo nvme-cli (2026-05-31..2026-06-06)

**Context**：Python harness 想真验 wire，最自然是 `subprocess` 调 nvme-cli。
但 nvme-cli `connect` 需要 root + `nvme-tcp.ko` 模块 + WSL2 kernel 兼容。

**Options**：
- A: sudo nvme-cli 包装（高真实度，前置门槛高）
- B: 纯 Python raw socket 发 NVMe-oF wire（自构造 PDU 全部字段）
- C: libnvme C library Python binding（依赖编译）

**Decision**：B（uv + stdlib only，0 sudo）。

**Consequences**：
- 7+1 Python scenarios 跑通，每个 < 200 LOC，纯 stdlib
- 不能验 Linux kernel nvme-tcp.ko 的 *实际* wire (只验自家算法)
- 真 kernel interop 留另一类 task (ROADMAP §1 real-host CHAP interop)
- *硬限*: Python harness 是 "我们自家算法对自家算法的高效 e2e"，不是 spec
  conformance test

**Status**：active；real-host interop task 是补足，不是替代。

---

## ADR-003 — V8e tokio runtime + AsyncSession (2026-06-06)

**Context**：V8 之前 session 是 sync thread-per-conn；扩到多 conn + AER +
KATO timer 需要 select。

**Options**：
- A: 继续 sync + 加 mpsc 桥 — KATO / AER 跨 thread 难
- B: tokio runtime + AsyncSession + select! — 重构成本高

**Decision**：B（V8e-1..V8e-7 7 sub-phase）。

**Consequences**：
- 引入 `#![deny(clippy::await_holding_lock)]` 防 lock-await-stall
- 落实 decision-then-IO 借用模式 (见 [LESSONS §11](/usnvmemu/docs/LESSONS.md))
- 性能：AER Notify < 10ms，KATO timer 精度 < 100ms
- bin 全 async，spawn_blocking 退役

**Status**：active。
