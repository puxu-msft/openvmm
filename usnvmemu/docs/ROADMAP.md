# usnvmemu Roadmap — 用户态 NVMe Firmware Emulator (动态文档)

> **维护策略**: 本文件 phase 完成时即时更新；废弃 phase 标 ~~strikethrough~~；
> 新 phase 按 "what / why / acceptance / blockers / commit links" 模板加。
> 历史上本文件叫 "NVMe-oF TCP Target Roadmap"，2026-06-08 升为项目级 (因
> firmware-as-core 愿景 + usnvmemu/ 独立)。

> **最后更新**: 2026-06-08 (usnvmemu/ 子目录化 + crate 改名 + 文档归属重组)

## 0. 当前坐标

```
V1 ──> V2 ──> V3 ──> V4abc ──> V5abcd ──> V5e/f-followup ──> V6abc ──> V7abc ──> V8a-f ──> V8e tokio ──>
V-followup-tls(1..4) ──> V-followup-mtls ──> V-followup-auth(1..2) ──> V-followup-dhchap(1..3,3-wire) ──>
V-followup-interop(1..7) ──> V-followup-prp-list ──> V-followup-dhchap-4 + 4d ──> V-interop-8 ──> [HERE]
```

**项目愿景** (用户 2026-06-06 explicit, [PROJECT_VISION.md](PROJECT_VISION.md))：
**用户态 NVMe firmware** 为核心 + 三种接入 (OpenHCL VTL2 / OpenVMM dev / QEMU vfio-user) +
NVMe-oF TCP 第 4 条接入。当前架构 ✅ 已对齐：firmware (controller) runtime-agnostic +
`trait Transport` 边界已抽出。

**当前状态**：
- 306 lib + integration tests pass，clippy 0 warning，#![forbid(unsafe_code)] + #![deny(clippy::await_holding_lock)] 维持。
- Linux nvme-cli plaintext discover + connect + IO 互通已实证。
- DHCHAP simplified wire + spec § 8.13.5 4-message wire 都通，多 descriptor 兼容。
- TLS / mTLS / NQN<->cert binding 端到端 (server-auth 全栈)。
- TLS PSK TP-8011 deterministic crypto 已落，**rustls 注入待上游**。

## 1. 短期 (1-3 phase, 不依赖上游)

> **firmware-as-core Tier 标签** (按 [PROJECT_VISION §4](PROJECT_VISION.md) 优先级)
> - **Tier 1**: 3 条接入各 1 个真 host e2e harness (本季)
> - **Tier 2**: firmware crate 命名重构 (下季)
> - **Tier 3**: Phase X 仓库拆分 (半年)

### Phase W — pcie_device_sdk 补全 hexagonal 结构 (Tier 2-结构, HIGH 优先, 分 W1-W4)

**What**：Phase T 只抽了出站半边，入站/描述/crate 边界仍泄漏 OpenHCL。把
`pcie_device_sdk` 拆成中立 `pcie_device_core` + 平级 transport adapter。详
[2026-06-08-phase-w-hexagonal-restructure.md](/usnvmemu/crates/pcie_device_sdk/docs/plans/2026-06-08-phase-w-hexagonal-restructure.md)
+ 决策 [ADR-010](/usnvmemu/crates/pcie_device_sdk/docs/DECISIONS.md)。

**Why**：教学 = 严谨全面（PRINCIPLES）。architect 复核确认 5 条结构债，最根本是
`DeviceDescribe` vs `Regions` **描述模型分叉**（同信息两套真相源）。修后"用不用
`vfio_user` crate"降级为局部可逆 adapter 私事。

**子阶段**：W1 中立描述模型（最高优先，顺带修 vfio-user cfg-space identity gap）→
W2 crate 拆分（撤 `pub use pcie_remote_protocol`）→ W3 device 层解耦（兑现 Phase T
deferred）→ W4 dma-completion 语义文档。

**显式不做**（ADR-010 否决）：对称入站 trait（PCI 读写本不对称）/ 单一 `generic run<T>()`（三 transport async 模型不同）。

**Acceptance**：见 plan §6。每子阶段过 rust-reviewer。

**关联 track（独立）**：vfio-user spec-complete（DMA head-of-line 阻塞 `dma.rs:260` /
mmap DMA / GET_REGION_IO_FDS）—— 不属 Phase W 结构范围，见 plan §8 + 下方
V-followup-vfio-user-qemu-harness。

### V-followup-dhchap-4-real-host-interop (Tier 1, HIGH 优先, 预计 1 day)

**What**：让 V-interop-8 Python harness 通过即代表 Linux nvme-cli `--dhchap-secret` 也通；
现在还缺一步：跑真 nvme-cli 而非 Python harness。

**Why**：Python harness 是我们自家算法对自家算法 (compute_response 两边都用同实现)，
真互通必须找 third-party host 验。

**Acceptance**：
- `sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 -n <subnqn> --hostnqn <hostnqn> --dhchap-secret DHHC-1:01:<base64key>:` 成功
- nvme-cli output 显示 "Connection established"
- 之后 `nvme list` 看得见 namespace

**Blockers**：
- Linux 6.6 WSL2 默认 kernel.modules 路径，nvme-cli 用户态 + nvme-tcp.ko 模块版本兼容
- DHHC-1 base64 key 格式 (kernel 用 ":sha256:" / ":sha384:" suffix)，文档化

**Links**：commit `714029df`, plans/2026-06-05-phase-v4-detailed.md §9 (entrypoint hint)。

### V-followup-tls-psk-kernel-vector (Tier 1, HIGH 优先, 预计 2 days, 独立于 rustls)

**What**：写 out-of-tree kernel module dump `nvme_auth_derive_tls_psk` 输出，固化
5 个 known-good (retained_psk, hostnqn, subsysnqn, hash, expected_tls_psk_hex)
五元组到 `src/tls_psk.rs::vt_tlspsk_kernel_ci_vectors_*` 测试。

**Why**：当前 13 tls_psk tests 全是 self-consistent，对自身 deterministic 但
*没有 anchor 到 Linux 真实输出*；任何 silent algorithm drift 都不会被发现。

**Acceptance**：
- `tls_psk.rs` 加 `vt_tlspsk_kernel_ci_vector_{1..5}_sha256` + `_{1,2}_sha384`
- 每个 test 用 hard-coded inputs 算出和 kernel 一致的 raw TLS PSK byte 串
- 之后任何 `tls_psk.rs` 代码变更必须 *先* 改 vector (CI red) *再* 改实现

**Blockers**：
- 需要 host root + 写小 kmod (~ 50 LOC)，用户须授权或代提
- WSL2 kernel 已开 `nvme-tcp` / `nvme-auth` 内置；EXPORT_SYMBOL_GPL OK

### ✅ V-followup-vfio-user-cross-process-harness (Tier 1, 2026-06-09 SHIPPED — 含真 QEMU 11 e2e)

**纠错（曾误判）**：早先基于 QEMU **8.2** 实测下结论"主线 QEMU 无 vfio-user
客户端、需 fork 编译"——**该结论错误**。vfio-user 客户端（`vfio-user-pci` 设备）
由 Nutanix/John Levon 在 **QEMU 10.1（2025-08）合入上游主线**。用 linuxbrew
**QEMU 11.0.1** 实测：`-device {"driver":"vfio-user-pci","socket":{...}}` 直接
realize 成功。教训记 [LESSONS.md](LESSONS.md) §21（"版本不带≠协议不支持"）。

**✅ 已做（三层 oracle，独立性递增）**：
- `vfio_user_transport/scripts/interop_py/`：Python stdlib vfio-user 客户端
  harness（`enumerate_smoke.py` 自包含 spawn+验+清理，13 断言：握手 / GET_INFO /
  全 9 region info / **CONFIG 读真验 W1 identity/class** / IRQ / BAR-probe / FLR）。
- **libvfio-user 官方 client 做 differential oracle**（需 `libjson-c-dev`）：独立
  C 实现验证我们协议层 + 抓到 2 个真 conformance bug（commit `533432ec`：bogus
  region 应 EINVAL / bulk REGION_READ），复现见 interop_py/README.md。
- **✅ 真 QEMU 11 e2e**（`scripts/qemu_interop/`，commit `b7d5adc4`）：真 QEMU
  以 `vfio-user-pci` realize 我们的 server，跑完整 VERSION/GET_INFO/GET_REGION_INFO/
  config 读/DMA_MAP 握手，QMP `query-pci` 确认 guest PCI 总线见 NVMe(0x1414/0xc0de)。
  抓到 2 个自家测试全绿的握手 bug（commit `f8fdf220`：version-minor 协商须
  `min(client,server)` / max_msg_fds 须 ≤ QEMU 16 上限）。
- DMA head-of-line 阻塞修复 + review H-1（reply flag 判据）`b1cb8574` + fixup。

**⏳ 诚实 defer（reviewer M-1/M-2/M-4 标注，待补）**：
- **bulk REGION_WRITE**：当前 WRITE 仅 1/2/4/8（`[0u8;8]` 假设）；补 config
  `write_bytes` + BAR chunk write（对称 READ 的 bulk）。
- **用协商 `max_data_xfer_size` 替代硬编码 4096**：需先在 `Negotiated` parse 出
  数值字段（当前只存 caps JSON 原始串）。
- **`region_access_ok` 缓存 BAR layout**：当前每次 region 访问 alloc describe() Vec。
- **mmap 零拷贝 DMA**：见 Phase W plan §8 设计（DmaTable 并行 mmaps + memfd 单测）。
- **真 guest OS 引导**：当前 QEMU e2e 用 `-S` 暂停 CPU，只验到 realize/PCI 枚举；
  引导 kernel+rootfs 看 `/dev/nvme0` 真读写需 IO queue DMA 全路径 + initramfs，留 future。

**Why**：[PROJECT_VISION §3.3](PROJECT_VISION.md) firmware-as-core 第 3 条接入。
真 QEMU 11 e2e 把 vfio-user 从"自家 client 验自家 server"推到"第三方独立实现
（真 QEMU）驱动协议全握手"——Tier 1"真 host e2e harness"目标达成。

### V-followup-discovery-multi-portal-real (Tier 2, MEDIUM, 预计 1 day)

**What**：实测 V7 / V8a `--discovery-target-addr` 重复指定多 portal，nvme-cli
discover 真能拿到多 entry。

**Why**：V7c-fix 是 lib test pass + reviewer 改的 CNTRLTYPE，但当时只测单
portal；多 portal 走 [[nvme-of-tcp-real-linux-interop-milestone]] 验过。

**Acceptance**：起 target with `--discovery-target-addr A:p1 -p A:p2 -p A:p3`，
`nvme discover -t tcp -a A -s 4420` 输出 3 个 entry，每个 NQN/IP/Port 正确。

### V-followup-py-harness-spec-wire-conformance (Tier 2, MEDIUM, 预计半 day)

**What**：把 `chap4_spec_wire_e2e.py` 扩到覆盖 reviewer M-4 那 5 个 case
(REPLY before challenge / REPLY truncation / tid mismatch / SUCCESS2 before
auth / FAILURE2 from host)，对齐 lib test。

**Why**：lib test 覆盖了，但 Python harness 缺；跨进程实证 wire 错误路径才能
保 Linux nvme-cli 拿到正确 FAILURE1 diagnostic。

### ✅ V-followup-firmware-rename (Tier 2, 2026-06-08 已 SHIPPED — crate 改名部分)

**What (已做)**：按 firmware-as-core 愿景重命名 crate (跟 usnvmemu/ 搬迁一起):
- `pcie_remote_nvme_userspace` → **`nvme_firmware`** ✅
- `pcie_remote_userspace_sdk` → **`pcie_device_sdk`** ✅
- `pcie_vfio_user_sdk` → **`vfio_user_transport`** ✅ (它是 transport backend 不是 SDK)
- `pcie_remote_rng_userspace` → **`rng_device_example`** ✅
- `pcie_remote_noop_host` → **`pcie_remote_test_harness`** ✅ (保留 pcie_remote_ 因专测该协议)
- `nvme_of_tcp_target` 保留 (名字已准确)

全 `git mv` 保 history；306 tests + clippy 0 warning 维持。

**What (仍未做, 留后续)**：
- `pcie_remote_protocol` → `pcie_remote_wire` — 该 crate 在主仓 `vm/devices/`，
  VTL2 path 还在用；等 Phase X3 仓库拆分一起动
- `docs/HOW_TO_ADD_TRANSPORT.md` — 教学加第 5 条 transport (e.g. iSCSI / NBD /
  toy stdio-pipe)。**Tier 2 剩余 HIGH 项**

### Phase X — 把项目搬出 openvmm 仓库 (Tier 3, MEDIUM-LARGE, 预计 1 week, 分 X1-X4 子段)

> **进度**: usnvmemu/ 子目录化 + crate 改名 (2026-06-08) 已完成 Phase X 的
> 内部重组部分 (相当于 X3 的"新 repo 结构"先在原仓内立好)。剩余 X1 (git dep
> 试水) / X2 (pal_async → tokio) / X4 (真独立 repo + vendor) 待做。

**What**：把 `usnvmemu/crates/*` 6 crate + 文档 + `vm/devices/pcie_remote_*`
搬出 openvmm，作为独立仓库。openvmm 仅留 VTL2 device 部分。

**Why**：教学项目独立有利于贡献者门槛 + CI 速度 + 文档/代码一体。

**调研已完成**：见 [2026-06-06-phase-x-extract-from-openvmm-survey.md](2026-06-06-phase-x-extract-from-openvmm-survey.md)。
结论：可行；耦合度比想象低 (7 example crate 早已 workspace `exclude`；
真用到的 openvmm 内部 crate 只 4 个；推荐路径 C 换 tokio + vendor)。

**Blockers**：
- `pal_async` 是 OpenHCL VTL2 必须；用户态可换 tokio (`nvme_of_tcp_target` 已证)
- workspace inheritance 让 git dep 单 crate 拉不下来；要么 git 整个 openvmm，
  要么 vendor

**Acceptance**：
- X1: `nvme_of_tcp_target` 单 crate 试水 git dep
- X2: pal_async → tokio (sdk + nvme userspace)
- X3: 新 repo 立 + git filter-repo 抽 + vendor 完成
- X4: openvmm 主仓只留 `vm/devices/pcie_remote_*` (or 也搬走)

## 2. 中期 (3-6 phase, 部分依赖上游)

### V-followup-tls-psk-rustls-wire (HIGH, depends rustls upstream)

**What**：rustls 出 stable external-PSK API 后立刻接：
- `src/tls.rs::build_acceptor_with_psk(store: PskStore) -> TlsAcceptor`
- `PskStore = HashMap<(Hostnqn, Subsysnqn), (raw_psk, hash)>`
- rustls PSK callback 内调 `tls_psk::{generate_psk_digest, derive_tls_psk, build_psk_identity}`
- `tests/vt_tls_psk_5_*` 系列 e2e (类似 V-followup-tls-3 的 5 个 file)
- `scripts/interop_py/tls_psk_e2e.py` 用 nvme-cli `--tls`

**Blocker**：rustls upstream（详 [tls-psk-survey](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)）。

### V-followup-fused-cmd (MEDIUM)

**What**：实现 NVMe Fused Compare-and-Write (CW + W 双 cmd 原子)；spec § 6.2。

**Why**：[[nvme-userspace-production-ready]] 教学 controller 标 "Fused detect"
但没真 atomic；nvme-of target 现在只对 hexadecimal opcode 0x06+0x09 拒。

### V-followup-zoned-namespace (LOW, 大块工作)

**What**：ZNS namespace support — Zone Append (0x7d) + Get Zone Receive Status。
需 backing layer 支持 zone state machine。

### V-followup-fabric-disconnect-real-interop (MEDIUM)

**What**：V8c Disconnect 真 Linux 实测；当前只测了 Python e2e。

## 3. 长期 (6+ phase, 架构级)

### V9 — RDMA Transport (HUGE, 6+ months)

**What**：spec § 5.13 NVMe-oF RDMA Transport；新 transport module `src/rdma_*`，
不复用 tcp_transport。

**Why**：spec 三大 fabric (TCP / RDMA / FC)，TCP 已 done；RDMA 是性能 ceiling。

**Architectural notes**：
- 不能复用 `framing.rs` (RDMA 不走 PDU)
- 需引 `rdma-core` Rust binding 或 fork ibverbs-sys
- AsyncSession 抽象层要再泛化 (TCP PDU vs RDMA SendRecv)
- 测试基础设施重写 (soft-RoCE in WSL2)

### V10 — DMA Backend (HUGE)

**What**：替换 `nvme_firmware` 教学 controller 的 file-backed
storage 到真 PCIe NVMe device，把 nvme-of target 变成 NVMe-oF JBOD gateway。

### V-spec-strict-mode (MEDIUM-LARGE)

**What**：增 `--strict-spec` flag 关掉所有 "教学版简化"，全跑 spec：
- DHCHAP DH ephemeral key exchange (DH-2048/4096/6144/8192)
- bidirectional CHAP auth (mutual auth + host-verify rval)
- secure channel concatenation (sc_c=1)
- spec-conformant Identify NS LBA Format full set
- ANA group state machine (ANAGRPID > 1)

**Why**：教学路径方便实验 + 简化，但 prod 部署必须 strict。

## 4. 当前 PRINCIPLES.md / LESSONS.md 入口

- **项目愿景** (firmware-as-core + 3 transport) → [PROJECT_VISION.md](PROJECT_VISION.md)
- 不可变约束 / coding policy → [PRINCIPLES.md](PRINCIPLES.md)
- 踩过的坑 + 经验 → [LESSONS.md](LESSONS.md)
- 重大决策 ADR → [DECISIONS.md](DECISIONS.md)
- TLS PSK 调研 → [2026-06-06-phase-v-followup-tls-psk-survey.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)
- 外部化调研 → [2026-06-06-phase-x-extract-from-openvmm-survey.md](2026-06-06-phase-x-extract-from-openvmm-survey.md)
- 各 phase 详 spec → `2026-06-0*-phase-*-detailed.md`

## 5. 流程提示 (本文件如何用)

- **下一个 phase 起手**：先 read 本 ROADMAP 找 HIGH 优先未 done 的；如果跨多
  phase 互相依赖，跑 `essentials:planner` 出 sprint plan
- **完成 phase 后**：本文件加 commit hash + acceptance evidence；MEMORY.md
  写 one-liner pointer；删 todo
- **新发现 phase**：写在合适优先段（短期/中期/长期），不堆"杂项"
- **废弃 phase**：strikethrough 不删，留 rationale 句

## 6. 历史 phase plan 索引 (2026-06-06 audit)

所有 `2026-XX-YY-phase-*.md` 文档顶都加了状态 banner。一览：

| Plan 文件 | 状态 | 备注 |
|----------|------|------|
| 2026-05-29-pcie-remote-impl.md | ✅ SHIPPED | PCIe Remote v2 (2026-05-30 已自标); 真 Linux KVM + Hyper-V e2e 通 |
| 2026-06-04-phase-t-transport-abstraction.md | ✅ SHIPPED | `trait Transport` 抽出，3 backend |
| 2026-06-04-phase-u-vfio-user.md | ✅ SHIPPED | U1-U5 + U-followup QEMU 接管模式 |
| 2026-06-04-phase-v-nvme-of-tcp.md | ✅ SHIPPED — 已大幅超越 | non-goals (TLS/CHAP) 全实现 |
| 2026-06-05-phase-v4-detailed.md | ✅ SHIPPED | V4a/b/c (R2T / H2CData / SGL) |
| 2026-06-05-phase-v5-detailed.md | ✅ SHIPPED | V5a-d + V5e1/2 + V-followup-prp-list 进一步提到 256 LBA |
| 2026-06-06-phase-v6-detailed.md | ✅ SHIPPED | V6a/b/c AER |
| 2026-06-06-phase-v7-short.md | ✅ SHIPPED | V7a/b/c-fix Discovery + V-interop 后续修 |
| 2026-06-06-phase-v8-detailed.md | ✅ SHIPPED | V8a/b/c/d/f (V8e 分到 tokio plan) |
| 2026-06-06-phase-v8e-tokio-detailed.md | ✅ SHIPPED | V8e-1..6 tokio runtime + Notify |
| 2026-06-06-phase-v8e-7-dispatch-detailed.md | ✅ SHIPPED | V8e-7-1..4 dispatch + bin 纯 async |
| 2026-06-06-phase-v-followup-tls-detailed.md | ✅ SHIPPED — 已大幅超越 | TLS server-auth → mTLS + NQN binding + 完整 CHAP 全栈 |
| 2026-06-06-phase-v-followup-prp-list-detailed.md | ⚠️ SUPERSEDED | 改走 session chunking；本文档原方案留给"future production PRP-list" |
| 2026-06-06-phase-v-followup-tls-psk-survey.md | 📋 CURRENT | rustls external-PSK 调研 + 决策路径 A+C |
| 2026-06-06-phase-x-extract-from-openvmm-survey.md | 📋 CURRENT | 外部化调研 + 决策推迟 (ADR-008) |

**未来 phase plan 命名约定**：`YYYY-MM-DD-phase-<name>-<detailed|short|survey>.md`。
- `detailed` = 落地实施细节 + 每段 sub-phase 拆解 + reviewer-pass 计划
- `short` = ≤ 100 行轻量 plan（如 V7-short）
- `survey` = 调研 / 决策类（如 tls-psk-survey）

落地后回本表加一行 + status icon (✅/⚠️/📋/❌)。

## 7. 无独立 plan 但已 shipped 的工作 (2026-06-06 audit 补)

下表是 `git log` 出现但 `plans/` 下没有独立 `.md` 的工作。它们落在
**别的目录的 README** 或 SESSION_LOG。**新加阶段不必每个都写 plan**，
小段可直接进对应 crate README + commit message + 本表登记。

| Phase 段 | 范围 | 文档归属 | commit 范围 |
|---------|------|---------|------------|
| Phase A..J (NVMe userspace 初版) | 单 PRP / SQ-CQ / dual-PRP / CRC PI | [`examples/nvme_firmware/README.md`](/usnvmemu/crates/nvme_firmware/README.md) | 早期 (SESSION_LOG 涵盖) |
| Phase K1..K9 (NVMe PI + Sanitize + Compare + Reservation) | T10 DIF + Compare PRP-list + NS Management + Sanitize + Doorbell Buffer + Reservation HOSTID | nvme userspace README | (SESSION_LOG 早期截止；后续段散见) |
| Phase L1..L5 (NVMe ZNS 基础 + Log Page + Directive + Security) | ZNS basics + Identify CNS 0x05/0x06 + Reservation Notification Log + Directive Send/Recv + Security Send/Recv | nvme userspace README | 2026-05-31..06-01 |
| Phase M1..M3 (NVMe IRQ coalesce + mmap + parallel) | Set Features 0x08 + mmap zero-copy + per-queue parallel ADR | nvme userspace README + `M2_MMAP_DESIGN.md` + `M3_PARALLEL_DESIGN.md` | `003bdb33`..`3658d21f` |
| Phase N1+N2 (SDK 测试 + 教学文档) | DeviceCtx outbound/inbound 单测 + NVMe 12-step lifecycle | nvme userspace README + `docs/NVME_LIFECYCLE.md` | 2026-06-01..02 |
| Phase O1..O3 (NVMe Copy + Fused C+W) | Simple Copy 0x19 + Fused dispatcher + atomic chain | nvme userspace README | `3bc36b26`..`c5af27fb` |
| Phase P1 (NVMe Endurance Group + NVM Set) | CNS 0x19 + CNS 0x04 + PTPL reservation | nvme userspace README | `0064e755` |
| Phase Q1..Q12 (NVMe 2.0 spec coverage 完整化) | PRACT + ZONE_APPEND PI + Telemetry + ANA + BP + Lockdown + crypto erase + RNM + DeviceCtx::for_testing + mod 拆分 + WSL→Windows MSVC 跨编 | nvme userspace README "## Phase Q 系列" | `e84578c9`..`cfd95e54` |
| Phase R1+R3+R4 (NVMe SGL + Identify advertise + RBAR) | SGL Data Block + sgls 字段 + RBAR ADR | nvme userspace README "## Phase R 系列" (本次 audit 补) | `f19c193f` |
| Phase S1..S7 (NVMe NS WP / NS Attach / Controller List / ANA state machine) | Write Protect + COPY conflict + Identify NS NAWUN 等 + NS Attachment 0x15 + Controller List CNS 0x12/13 + Reservation Notification Log + ANA state machine + Change AEN | nvme userspace README "## Phase S 系列" (本次 audit 补) | `d0b36b19`..`f8d847ea` + `10f987f5` |
| K-20 hotplug (pcie_remote) | listener 永不退 + worker transport refresh | [`../PCIE_REMOTE_HOTPLUG_DESIGN.md`](/usnvmemu/docs/pcie-remote-phase/HOTPLUG_DESIGN.md) (本次 audit 修正) + SESSION_LOG | `a99cdc63` + `64da8fb6` + `93c5fa5f` |
| Phase I3 (RNG example) | 第二个 PcieDevice 教学 example | [`../examples/rng_device_example/README.md`](/usnvmemu/crates/rng_device_example/README.md) (本次 audit 补) | `7402dd62` |

**判据 — 何时写独立 plan，何时跳过**：
- 写 plan: > 1 day 工作 + 跨多 module + reviewer round 可能 ≥ 2 轮 + 决策点不止 1 个
- 跳过 plan: ≤ 1 day 单点 feature + 单 module + 决策已明 + commit message 能覆盖

跳过 plan 不代表跳过文档；**对应 crate README + commit message 必须详细到能让人复现**。
