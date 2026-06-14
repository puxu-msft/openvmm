# NVMe-oF TCP target — 概览与导览

> **本文体裁**：这是一篇**自上而下的导览（reader's guide）**，给第一次接触这条线的读者
> （人，或一个无上下文的新 Claude 会话）一个全貌入口——**是什么、整体怎么运转、现在到哪了、
> 想深入该读哪篇**。它**不**重复过程细节：每个 phase 的 commit/弯路在 [MILESTONES §4](MILESTONES.md)，
> 每个 phase 的设计在 [`crates/nvme_of_tcp_target/docs/plans/`](../crates/nvme_of_tcp_target/docs/plans/)，
> wire 字节布局在 [wire-reference spec](../crates/nvme_of_tcp_target/docs/specs/2026-06-04-nvme-tcp-wire-reference.md)，
> 跨切面决策在 [crate DECISIONS](../crates/nvme_of_tcp_target/docs/DECISIONS.md)（ADR-003..007），
> 动态待办在 [ROADMAP](ROADMAP.md)，最新状态在 auto-memory `nvme-of-tcp-current-state`。
> **逐 flag 的 build/run/interop 操作手册**见 [crate README](../crates/nvme_of_tcp_target/README.md)
> ——本文与 README 分工：**本文讲「为什么 / 怎么运转 / 在哪」，README 讲「怎么 build、怎么开某个 flag」**。
>
> **最后更新**：2026-06-13（V-series 冻结于 V-interop-8；全栈 shipped，306 tests）。

---

## 1. 一句话

把 usnvmemu 的用户态 **NVMe firmware（`NvmeController`）暴露成一个标准 NVMe-over-Fabrics TCP target**：
一个普通 Linux/Windows 主机用**未打补丁的 `nvme-cli`**（`nvme connect -t tcp`）就能挂载它、跑真 IO，
就像连一台真的 NVMe-oF 存储。它是 firmware-as-core 的**第 4 条接入**（前三条走 PCIe：OpenHCL vsock /
OpenVMM / vfio-user），唯一**不经 PCIe、走网络 fabric** 的那条。已真机实证：真 Linux `nvme-tcp.ko`
（kernel 6.6.114）plaintext discover + connect + IO 双向互通。

## 2. 是什么（拆解）

- **NVMe-oF（NVMe over Fabrics）** —— NVMe 命令集**脱离 PCIe**、跑在网络 fabric 上的标准（spec NVMe
  Base 2.0 + TCP transport binding spec § 8.13）。host 与 target 间不再是 PCIe 寄存器/DMA，而是
  **capsule**（命令/响应胶囊）+ **data PDU** 在 TCP 流上往返。
- **TCP transport** —— fabric 的一种具体承载。每条 TCP 连接上跑一套 PDU 协议：`ICReq/ICResp` 握手 →
  `CapsuleCmd/CapsuleResp` 收发命令 → `H2CData`（host→controller 写数据）/ `C2HData`（controller→host
  读数据）/ `R2T`（Ready-to-Transfer，target 向 host 要写数据）。每个 PDU 带可选 CRC32C digest。
- **target** —— fabric 的「设备端」。本 crate `nvme_of_tcp_target` 就是 target：listen TCP :4420，
  每条 conn 解析 PDU、把 capsule 里的 NVMe 命令喂给后端 `NvmeController`，把 controller 的 CQE/数据
  转回 PDU。
- **与 firmware core 的关系（关键）** —— target **不自己实现 NVMe 语义**，它只做 wire ↔ command 的
  翻译，真正的 admin/IO 命令处理全部委托给 [`nvme_firmware::NvmeController`](../crates/nvme_firmware/)
  ——那是 runtime-agnostic 的同一份 controller，三条 PCIe 接入也用它。NVMe-oF 这条线**证明了 controller
  核足够 transport-agnostic**：连「没有 PCIe、没有 guest 物理内存」的网络 fabric 都能复用它（见 §5 ①）。

**为什么有价值**：① 用一份零拷贝/零改动的 firmware core 同时支撑 PCIe 与 fabric 两类截然不同的 transport，
是 firmware-as-core 愿景的最强证据；② 标准 `nvme-cli` 即可互通 = 真实可用而非玩具；③ 教学——完整展示
NVMe-oF TCP wire 协议 + R2T 流控 + fabric 认证栈（TLS/mTLS/DH-HMAC-CHAP）。

## 3. 全栈数据流（已真机实证）

```
   host（Linux nvme-tcp.ko / Windows nvme-cli）
       │  把 target 当真 NVMe-oF 存储；nvme connect -t tcp -a <ip> -s 4420
       ▼
   TCP :4420 （可选 TLS :8009）           ← capsule + data PDU 走 TCP 流
       │  每 PDU 带可选 CRC32C digest（framing.rs）
       ▼
   nvme_of_tcp_target （bin：tokio::main，每 conn 一个 tokio::spawn）
   ┌──────────────────────────────────────────────────────────────┐
   │ AsyncSession  ── pump_one_async() select! 4-arm 主循环 ──┐     │
   │   │  shutdown / AER notify / KATO deadline / read PDU   │     │
   │   ├ Fabric Connect / Property Get/Set        (fabric.rs)│     │
   │   ├ CapsuleCmd 派发  ── decide_*() 纯函数决策核 (dispatch_plan.rs)
   │   ├ 写: R2T → H2CData → 重组 → controller.dma_read 完成  │     │
   │   └ 读: controller.dma_write 被 capture → C2HData PDU   │     │
   └───────────────────────┼──────────────────────────────────────┘
                           │  「假 GPA」DMA-capture 桥（§5 ①）
                           ▼
   nvme_firmware::NvmeController  （runtime-agnostic，三 PCIe 接入共享同一份）
       │  admin / IO 命令真处理；post_cqe / dma_read / dma_write 以 GPA 表达
       ▼
   backing file（普通 regular file，每 --backing-file 一个 namespace）
```

**关键点**：target 与 controller 之间没有真 guest 物理内存——session 用**哨值 GPA** 截获 controller 发出的
DMA，翻成 fabric data PDU（见 §5 ①）。这是「网络 fabric 复用一个为 PCIe DMA 设计的 controller 核」的接缝。

## 4. 关键代码拓扑

crate 根：[`usnvmemu/crates/nvme_of_tcp_target/`](../crates/nvme_of_tcp_target/)（src ~11k 行）。

| 路径 | 角色 |
|---|---|
| [src/bin/nvme_of_tcp_target.rs](../crates/nvme_of_tcp_target/src/bin/nvme_of_tcp_target.rs) | 长跑 bin 入口：`#[tokio::main(multi_thread)]` + dual-listener（IO/Discovery）+ `SharedControllerInner`（`Arc<parking_lot::Mutex<NvmeController>>` 多 conn 共享）+ CLI flag 解析 |
| [src/async_session.rs](../crates/nvme_of_tcp_target/src/async_session.rs)（2269） | `AsyncSession`：`pump_one_async` 的 `tokio::select!` 4-arm 主循环（async_session.rs:326）；bin 主路径 |
| [src/session.rs](../crates/nvme_of_tcp_target/src/session.rs)（3511） | `V2Session` sync 核 + **假-GPA DMA-capture 桥**（session.rs:48-69 的 `CQ_BASE_GPA` / `PRP1_SENTINEL`）+ `NvmeController` 集成（session.rs:41）；sync 路径保留作集成测试参考 |
| [src/dispatch_plan.rs](../crates/nvme_of_tcp_target/src/dispatch_plan.rs) | **sans-IO 决策核**：`decide_capsule_kind / decide_admin_aer_path / decide_io_nlb_check / prp2_sentinel_for_nlb` 等纯函数，sync/async 两路径共用防漂移 |
| [src/pdu.rs](../crates/nvme_of_tcp_target/src/pdu.rs) / [framing.rs](../crates/nvme_of_tcp_target/src/framing.rs) / [digest.rs](../crates/nvme_of_tcp_target/src/digest.rs) | wire PDU 类型 + `read_pdu(_async)`/`write_pdu(_async)` 编解码 + CRC32C digest（sync/async byte-exact，regression gate 锁定） |
| [src/r2t.rs](../crates/nvme_of_tcp_target/src/r2t.rs) / [h2c_reassembler.rs](../crates/nvme_of_tcp_target/src/h2c_reassembler.rs) / [ttag.rs](../crates/nvme_of_tcp_target/src/ttag.rs) | 写路径：`encode_r2t` 发 R2T、`H2cReassembler` 重组多段 H2CData、`TtagAllocator` 分配 transfer tag（wrap 跳过 0） |
| [src/fabric.rs](../crates/nvme_of_tcp_target/src/fabric.rs) | Fabric `Connect`（HOSTNQN/SUBNQN/qid）+ `Property Get/Set`（controller 寄存器经 fabric 读写） |
| [src/tls.rs](../crates/nvme_of_tcp_target/src/tls.rs) / [tls_identity.rs](../crates/nvme_of_tcp_target/src/tls_identity.rs) | `build_acceptor_from_pem`（server-auth TLS）/ `build_acceptor_with_mtls`（强制 client cert）/ `extract_host_identities`（NQN↔cert SAN/CN binding） |
| [src/tls_psk.rs](../crates/nvme_of_tcp_target/src/tls_psk.rs)（525） | TP-8011 PSK deterministic 派生层（digest + HKDF-Expand-Label）——**未注入 rustls 握手**（见 §6 / ADR-007） |
| [src/dhchap.rs](../crates/nvme_of_tcp_target/src/dhchap.rs)（1020） | DH-HMAC-CHAP in-band auth：simplified 2-msg + spec § 8.13.5 4-msg 双 wire 共存（首发包自动锁定，ADR-005） |
| [crates/nvme_firmware/](../crates/nvme_firmware/) | **后端**：runtime-agnostic `NvmeController`（三 PCIe 接入也用它） |
| [crates/pcie_device_core/](../crates/pcie_device_core/) | 零依赖 domain core（Phase W 抽出，controller / transport 共享类型） |

> **workspace 归属**：`nvme_of_tcp_target` 与所有 usnvmemu crate 一样在根 `[workspace.exclude]`，
> **不在 openvmm 主 CI**——但已有 **usnvmemu 专属 CI gate**（commit `684f43054`）让其独立 oracle
> 真 standing。本地手测：`cd usnvmemu/crates/nvme_of_tcp_target && cargo test`（根 `-p` 报包不存在）。
> 见 auto-memory `usnvmemu-no-ci-gate-workspace-excluded`。

## 5. 关键模型

### ① 「假 GPA」DMA-capture 桥（最核心，是 fabric 复用 PCIe controller 的接缝）

`NvmeController` 是为 PCIe 设计的：它处理命令时用 `post_cqe` / `dma_read(gpa, len)` / `dma_write(gpa, buf)`
把完成项和数据**以 guest 物理地址（GPA）表达**。但 NVMe-oF 没有 guest、没有真物理内存。session 的解法
（[session.rs:48-69](../crates/nvme_of_tcp_target/src/session.rs)）：

- 给 controller 安装一个**「假」admin CQ**，base_gpa = 哨值 `CQ_BASE_GPA`；controller `post_cqe` 写
  `cq_base + slot*16` 时，session 识别 `gpa ≥ CQ_BASE_GPA` ⇒「这是 CQE 字节」→ 转 `CapsuleResp`。
- 给数据 PRP 安装哨值 `PRP1_SENTINEL`；controller `dma_write(prp1, data)` ⇒ session 识别为「应答数据」
  → 转 `C2HData` PDU（读路径）；`dma_read(prp1, len)` ⇒ session 发 `R2T` 向 host 要写数据，host 回
  `H2CData`，重组后调 `on_dma_complete` 把字节喂回 controller（写路径）。

即：**session 不改 controller 一行，靠拦截 controller 的 GPA-DMA 把它「骗」进 fabric wire**。这正是
controller 核 transport-agnostic 的实证。

### ② PDU / framing wire

每条 conn 一套 PDU 序列：`ICReq/ICResp`（连接初始化协商 MAXH2CDATA 等）→ `CapsuleCmd`（64B SQE +
可选 in-capsule data）/ `CapsuleResp`（16B CQE）→ `R2T` / `H2CData` / `C2HData`。framing 层
（[framing.rs](../crates/nvme_of_tcp_target/src/framing.rs)）负责 PDU 头 + 可选 CRC32C digest
（[digest.rs](../crates/nvme_of_tcp_target/src/digest.rs)）；sync 与 async 编解码**字节完全一致**
（regression gate 锁定）。**字节布局以 spec 为准**，逐字段查源/`offset_of!` 锚定，**禁手算 packed
offset**（V-interop 阶段手算 offset 全错的血泪教训，见 §6 / LESSONS §17）。

### ③ R2T / H2CData / C2HData 数据传输状态机

- **读**（IO Read / admin 返数据）：controller `dma_write(prp1, buf)` → session capture → 切成
  `C2HData` PDU + 末尾 `CapsuleResp`。
- **写**（IO Write / admin `dma_read` 如 NS Attachment）：controller `dma_read(prp1, len)` → session 按
  MAXH2CDATA 切成多个 `R2T`（每个带唯一 transfer tag）→ host 回多段 `H2CData` → `H2cReassembler`
  重组 → `on_dma_complete` → `post_cqe`。
- async 路径下 R2T 是**三段式 lock-pop / unlock-await-wire / lock-complete**（持 lock 不能 await，
  `#![deny(clippy::await_holding_lock)]` 强制；decision-then-IO 借用模式，见 LESSONS §11）。

### ④ AsyncSession tokio `select!` 4-arm 主循环

bin 主路径每条 conn `tokio::spawn(handle_conn_async)` → `pump_one_async`
（[async_session.rs:326](../crates/nvme_of_tcp_target/src/async_session.rs)）多路复用四件事：
**shutdown watch / AER notify（`Arc<Notify>`，端到端 < 10ms）/ KATO deadline（`tokio::time::Sleep`，
spec § 7.13 keep-alive 超时）/ read_pdu_async**。返 `PumpEvent`（async_session.rs:424）。controller 侧仍
`parking_lot::Mutex` 短锁串行（async-aware lock 留 future），但 wire 层多 conn 真并发（ADR-003）。

### ⑤ sans-IO 决策核（dispatch_plan）

把「这个 capsule 该走哪条路」抽成**纯函数**（[dispatch_plan.rs](../crates/nvme_of_tcp_target/src/dispatch_plan.rs)）：
`decide_capsule_kind / decide_admin_aer_path / decide_admin_discovery_whitelist / decide_admin_blocked_opc /
decide_io_nlb_check / prp2_sentinel_for_nlb`。sync `V2Session` 与 async `AsyncSession` **共用同一决策核**，
防两路径行为漂移——这是「双轨实现共享单一真相」的可复用模板。

### ⑥ Fabric 认证栈（分层，各 flag 正交可叠加）

从弱到强：明文（默认 loopback-only）→ host NQN 白名单（`--allow-host-nqn`，**仅防配错、非认证**）→
TLS server-auth（`--tls-*`）→ mTLS 强制 client cert（`--tls-client-ca`）→ NQN↔cert SAN/CN binding
（`--tls-bind-nqn-to-cert`，spec § 8.13 推荐的真 host 认证）→ in-band DH-HMAC-CHAP（`--host-secret`）。
逐 flag 的开法/openssl 配方见 [README](../crates/nvme_of_tcp_target/README.md)。**各档的教学/生产边界见 §6**。

## 6. 教学/生产边界（诚实）

本线是**教学但生产级**：wire 与命令路径对 `nvme-cli` 真实可用，但部分**安全栈是教学版简化**，绝不冒充生产
完整。诚实标注（同 auto-memory `nvme-of-tcp-current-state` / [MILESTONES §4.13](MILESTONES.md)）：

- **DH-HMAC-CHAP 是 HMAC-only**——无 DH ephemeral key exchange ⇒ **无 forward secrecy**；单向（host→target，
  无 mutual auth）。transcript 含 `(challenge‖hostnqn‖subnqn)` 防 cross-replay、constant-time verify、
  OsRng nonce 都做了，但不是完整 spec § 8.13.5 的 DH 增量。
- **TLS PSK（TP-8011）只有 deterministic crypto，未注入 rustls 握手**——`tls_psk.rs` 的 digest +
  HKDF-Expand-Label 派生层 field-by-field 对齐 Linux master `drivers/nvme/common/auth.c`，但 rustls 0.23
  无公开 external-PSK API，PSK **进不了真 TLS 握手**（ADR-007：不 fork rustls，等上游）。
- **`tls_psk.rs` 测试是 self-consistent**——对自身 deterministic，但缺 Linux kernel 真五元组 anchor
  （known-good `nvme_auth_derive_tls_psk` 输出）。self-consistent ≠ spec-conformant（项目反复踩的坑，
  见 auto-memory `review-not-optional-self-consistent-trap`）；补 anchor 是 ROADMAP §1 HIGH。
- **`parse_negotiate` 只看 DHCHAP authid**（跳过其他 auth family）——教学版 OK。
- **IO 单 cmd ≤ 128 KiB（256 LBA）**——靠 session-level 透明 chunking（16→256 LBA）达成，**不动 controller
  的 PRP-list path**（ADR-006：B 方案 200 LOC vs A 方案 500 LOC 跨 lib）；> 256 LBA 返 SC=0x18。
  future production 路径仍是 controller PRP-list（ADR-006 保留作起点）。
- **Python interop harness 是「自家算法对自家算法」**，不是 kernel nvme-tcp.ko 的 spec conformance
  test（ADR-004）；真 kernel 互通是另一类 task（ROADMAP §1 real-host CHAP）。

> 下一步 HIGH（[ROADMAP §1](ROADMAP.md)）三项**都卡外部依赖**：real-host CHAP interop（卡 WSL2 kernel
> `CONFIG_NVME_AUTH` 未编，见 MILESTONES §4.11 + RUNBOOK_HOST_ROOT.md §1）/ kernel-CI 五元组 anchor /
> TLS PSK rustls wire 注入（等 rustls upstream external-PSK API）。

## 7. 当前状态

**V-series 全栈 shipped、已冻结于 V-interop-8**（[ROADMAP §0](ROADMAP.md)，2026-06-13 更新）：

- **306 lib + integration tests pass，clippy 0 warning**；`#![forbid(unsafe_code)]` +
  `#![deny(clippy::await_holding_lock)]` 维持。冻结点 commit `714029df1`（V-followup-dhchap-4d +
  V-interop-8）。
- **真 Linux nvme-cli plaintext discover + connect + IO 互通已实证**（kernel `nvme-tcp.ko` 6.6.114，
  WSL2；修了 9 个 wire blockers，[MILESTONES §4.11](MILESTONES.md)）。
- **DHCHAP simplified + spec § 8.13.5 4-msg wire 都通**，多 descriptor 兼容（commit `eff95619` +
  `714029df1`）。
- **TLS / mTLS / NQN↔cert binding 端到端**（server-auth 全栈）；**TLS PSK TP-8011 deterministic crypto
  已落，rustls 注入待上游**（commit `43040427`）。
- **Fused Compare-and-Write over fabric 真原子**（commit `79daadc13`）；**纯-4K LBAF over fabric**；
  **PRP-list session chunking 16→256 LBA**（commit `2a4d734b`）。
- **跨进程 Python harness 22 scenarios 通过**（uv-managed venv，stdlib only，0 sudo；commit
  `57d12a7b8` 为 spec-wire-conformance 收尾）。

此后项目重心转向 **firmware-as-core 三 PCIe 接入真机化**（见 auto-memory `vfio-user-underhill-state`），
NVMe-oF 大体冻结于此状态。**后续 todos 都卡外部依赖**（见 §6 末 + ROADMAP §1）。

## 8. 文档地图（深读指引）

| 想了解 | 读这篇 |
|---|---|
| **逐 flag build / run / nvme-cli interop / 每个安全开关怎么开** | [crate README](../crates/nvme_of_tcp_target/README.md)（使用手册，含 openssl/nvme-cli 配方） |
| **每个 V phase 的 commit + 带注解弯路 + 9 个 wire blocker** | [MILESTONES.md §4](MILESTONES.md)（4.1–4.13，V4 → V-followup） |
| **wire 字节布局（PDU 各字段 offset）** | [specs/2026-06-04-nvme-tcp-wire-reference.md](../crates/nvme_of_tcp_target/docs/specs/2026-06-04-nvme-tcp-wire-reference.md) |
| **跨切面决策（为什么这样收口）** | [crate DECISIONS.md](../crates/nvme_of_tcp_target/docs/DECISIONS.md)（ADR-003 tokio / 004 Python harness / 005 双 CHAP wire / 006 session chunking / 007 不 fork rustls）；跨项目 ADR 见 [项目 DECISIONS](DECISIONS.md) |
| **TLS PSK 为什么不注入 rustls** | [plans/2026-06-06-phase-v-followup-tls-psk-survey.md](../crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md) |
| **各 V phase 详细设计** | [crate docs/plans/](../crates/nvme_of_tcp_target/docs/plans/)（V4/V5/V6/V7/V8/V8e/V-followup-tls 等，顶部带 status banner） |
| **真 host nvme-cli 命令 / CHAP 硬阻塞出路** | [RUNBOOK_HOST_ROOT.md](RUNBOOK_HOST_ROOT.md) |
| **NVMe-oF 在愿景中的位置（第 4 接入）** | [PROJECT_VISION.md](PROJECT_VISION.md) |
| **当前 live 状态 / 下一步 HIGH** | [ROADMAP.md](ROADMAP.md) §0/§1 + auto-memory `nvme-of-tcp-current-state` |
| **踩坑教训（self-consistent 陷阱 / async lock / 手算 offset）** | [LESSONS.md](LESSONS.md) §17–§30+ |

## 9. 怎么跑（最小路径）

```bash
cd usnvmemu/crates/nvme_of_tcp_target
truncate -s 1G /tmp/ns1.img                       # 1 GiB 空 backing file
cargo run --release -- \
  --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img

# 另一终端：真 Linux nvme-cli（kernel ≥ 5.0）
sudo modprobe nvme_tcp
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 \
  -n nqn.2026-06.io.openhcl:nvme.userspace
sudo nvme list                                    # 期望见 /dev/nvme0n1
sudo dd if=/dev/zero of=/dev/nvme0n1 bs=4k count=1 oflag=direct
sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace
```

TLS / mTLS / DH-HMAC-CHAP / 多 namespace / Discovery 等开关的完整命令（含 openssl 生成 cert、
`nvme-cli --tls` 用法、`--host-secret` 配 DHCHAP），见 [crate README](../crates/nvme_of_tcp_target/README.md)
的「Build & Run」「Linux nvme-cli interop」「TLS / mTLS / DH-HMAC-CHAP 教学开关」各节。
无 sudo 的跨进程 wire 实验见 [`scripts/interop_py/`](../crates/nvme_of_tcp_target/scripts/interop_py/)。
