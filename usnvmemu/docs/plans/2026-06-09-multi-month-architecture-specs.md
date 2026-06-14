# Specs: 多月架构级任务（新会话满上下文分阶段执行）

> 2026-06-09。这些是 ROADMAP §2/§3 的大块工作（数周~数月）。**不是不做，是单会话
> 不宜 rush**——每个给 what / why / 分阶段 plan / 关键架构决策 / 风险，让新会话从某个
> 子阶段起手，不必从零摸索。每个子阶段独立可 commit + review。

---

## R. real-guest-boot —— vfio-user 全-DMA e2e（预计 3-5 day，**最近且解锁 mmap 验证**）

### Why 优先
当前 vfio-user QEMU harness 用 `-S` 暂停 CPU，只验到 device realize + PCI 枚举。
mmap 零拷贝 DMA（commit dfa9fefb）的**全路径只由 memfd 单测验证**，没被真 DMA 流量
跑过。引导真 guest 看 `/dev/nvme0` 真读写 = vfio-user 的终极 e2e + mmap 性能/正确性
的真实验证。

### 分阶段
1. **最小 initramfs + kernel**：WSL2 已有内核源/镜像；或用 `virt-builder` / busybox
   initramfs（含 nvme + nvme-core 模块）。
2. **QEMU 引导**（去掉 `-S`）：`scripts/qemu_interop/run_qemu_vfio.py` 加 `-kernel`
   + `-initrd` + `-append "console=ttyS0 ..."`，串口抓 guest dmesg。
3. **断言**：guest dmesg 见 `nvme nvme0: pci function` + `/dev/nvme0n1` 出现；
   `dd if=/dev/nvme0n1` / `nvme id-ctrl /dev/nvme0` 成功。
4. **mmap 路径确认**：server 日志（RUST_LOG=debug）见 "DMA_MAP mmap zero-copy attached"
   + dma_read/write 走 mmap_read/write（非 wire）。这是 mmap 全-e2e 验证。
5. **IO 正确性**：guest 写已知 pattern → server 端 backing-file 见同 pattern（跨 mmap DMA）。

### 风险
- WSL2 nested KVM 已确认可用（QEMU harness 已用 accel=kvm:tcg）。
- guest NVMe driver 对 BAR/MSI-X/admin queue 的要求比 realize 严——可能暴露新 wire
  blocker（这正是价值：第三方 driver 是更强 oracle）。按 §20/§22 用真字节诊断。

---

## V9. RDMA Transport（spec §5.13，预计 6+ 月，HUGE）

> **2026-06-14 升级**：本段已被**正式 detailed plan 取代** →
> [2026-06-14-phase-v9-rdma-detailed.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-14-phase-v9-rdma-detailed.md)
> （3 路 subagent 审计 + architect review 过；协议 8/8 经 Linux 真驱动 CONFIRM；3 处设计错误纠正 + R0-R5+M2
> 分阶段 + mock-first）。**下方原始粗略分阶段保留作历史**；以 detailed plan 为准。**注**：本段原文"AsyncSession
> 再泛化"已细化为 `FabricBackend`+`RdmaVerbs` 两层 trait；"soft-RoCE in WSL2 做无硬件 e2e"已坐实**当前内核
> 未编 rxe/siw、无纯用户态出路**（须重编内核，推迟到 R5）。

### What / Why
NVMe-oF 三大 fabric（TCP/RDMA/FC），TCP 已 done。RDMA 是性能 ceiling。新 transport
module `src/rdma_*`，**不复用** `tcp_transport.rs`（RDMA 不走 PDU/PDU framing）。

### 关键架构决策（先定，否则返工）
1. **rdma-core binding**：用 `rdma-core` Rust crate 还是 fork `ibverbs-sys`？先调研
   crates.io 现有 ibverbs binding 的维护度/安全性（遵循 development-workflow：GitHub
   search first）。
2. **AsyncSession 再泛化**：当前 `AsyncSession`（async_session.rs）耦合 TCP PDU 收发。
   RDMA 是 SendRecv + RDMA Read/Write verbs，不是 byte-stream。需把 session 的"收一个
   capsule / 发一个 response / 做 data transfer"抽象成 trait，TCP 和 RDMA 各实现。
3. **soft-RoCE 测试基础设施**：WSL2 装 `rdma-core` + `rxe` (soft-RoCE) 做无硬件 e2e。
4. **Transport trait 复用**：RDMA 的 `RdmaTransport` 仍 `impl pcie_device_core::Transport`
   （dma_read→RDMA Read verb / dma_write→RDMA Write verb / fire_interrupt→completion）。
   见 [HOW_TO_ADD_TRANSPORT.md](/usnvmemu/docs/HOW_TO_ADD_TRANSPORT.md) §6 fabric 模式。

### 分阶段（粗）
- R1: ibverbs binding 选型 + soft-RoCE WSL2 环境 + "hello QP" smoke。
- R2: AsyncSession 泛化（抽 SessionIo trait，TCP 先迁过去保持绿）。
- R3: RDMA Connect/admin capsule over SendRecv。
- R4: RDMA Read/Write verbs 做 data transfer（替 R2T/H2CData/C2HData）。
- R5: 真 `nvme connect -t rdma` 互通（host-root）。
- 每阶段独立 review；unsafe（verbs FFI）必过 reviewer + SAFETY 用独立 oracle（§22）。

### 风险
- ibverbs FFI 是大片 unsafe——按 §22 标准每个 SAFETY 用真 oracle，别 self-consistent。
- 内存注册（MR）生命周期 vs DMA region——比 vfio-user mmap 更复杂的 fd/内存所有权。

---

## V10. DMA Backend（HUGE）——替教学 controller 的 file-backed storage 为真 PCIe NVMe

### What / Why
把 `nvme_firmware` 教学 controller 的 file-backed storage 换成**真 PCIe NVMe device**
（vfio passthrough / SPDK-style），让 nvme-of target 变成 **NVMe-oF JBOD gateway**。

### 关键决策
1. **真设备访问**：Linux vfio-pci 把真 NVMe BAR mmap 到用户态？还是走 `/dev/nvme0`
   passthrough（`nvme io-passthru`）？前者性能高但 unsafe 大；后者简单但慢。
2. **backing trait 抽象**：当前 controller 的 backing 是 `BackingStore` trait？先核
   nvme_firmware 的 storage 抽象边界，新增 `RealNvmeBacking` impl，不动 controller core。
3. **DMA 直通**：guest DMA region ↔ 真设备 DMA——是否能零拷贝直连（避免 double-copy）。

### 分阶段
- D1: 核 controller 的 backing 抽象边界（可能要先 refactor 出干净 `BackingStore` trait）。
- D2: `nvme io-passthru` based `RealNvmeBacking`（慢但正确，先跑通）。
- D3: vfio-pci mmap 直通（性能，大片 unsafe，§22 标准）。
- D4: 真硬件 e2e（需有真 NVMe 盘 + vfio 绑定 = host-root + 硬件）。

### 风险
- 需真 NVMe 硬件 + root（vfio-pci bind）——host-root + 硬件双重阻塞。
- 教学 vs 生产边界：真设备直通离"教学严谨"远，离"生产 gateway"近——确认这符合项目愿景再投。

---

## V-spec-strict-mode（MEDIUM-LARGE）——`--strict-spec` 关掉所有教学简化

### What
加 flag 关掉所有"教学版简化"，全跑 spec：
- DHCHAP DH ephemeral key exchange（DH-2048/4096/6144/8192，当前只 DHGROUP_NULL）。
- bidirectional CHAP（mutual auth + host-verify rval，当前单向）。
- secure channel concatenation（sc_c=1）。
- spec-conformant Identify NS LBA Format full set。
- ANA group state machine（ANAGRPID > 1）。
- **CHAP transcript 用 spec wire format 而非 `||` 串接**（dhchap.rs:75，real-host 互通也需要）。

### 分阶段（每条独立）
- S1: CHAP spec-wire transcript（也解锁 RUNBOOK §1 dhchap-4 真互通）← **优先，最小**。
- S2: DH ephemeral（DHGROUP_2048…）——需引 DH 大数运算 crate。
- S3: bidirectional CHAP。
- S4: Identify LBA Format full set / ANA state machine。

### Why S1 优先
CHAP `||` 串接 vs spec wire 是 real-host 互通的已知差异点（见 RUNBOOK §1 已知风险）。
做 S1 不仅是 strict mode，还直接让 dhchap-4 真 nvme-cli 互通成为可能。**建议把 S1 从
本 spec 提出来，作为 dhchap-4 互通的前置单独做**（纯代码，可无人值守）。

---

## V-zoned-namespace（LOW，大块）——ZNS support

### What
Zone Append（0x7d）+ Get Zone Receive Status；需 backing layer 支持 zone state machine
（empty/open/closed/full + write pointer）。

### 分阶段
- Z1: backing 加 zone state machine（per-zone WP + state 转移）。
- Z2: Zone Management Receive（report zones）。
- Z3: Zone Append + 隐式 open/close 状态转移。
- Z4: Identify NS ZNS-specific fields（zsze / zoc / mar / mor）。

### 风险
- 是独立大 feature，与 fabric/transport 正交；可单独 crate-feature 门控。
- 优先级 LOW——除非有教学/演示需求，排在 RDMA/spec-strict 之后。

---

## 建议执行顺序（新会话起手）
1. **S1（CHAP spec-wire）** —— 纯代码、解锁 dhchap-4 互通、最小。
2. **R（real-guest-boot）** —— 解锁 vfio mmap 全验证、3-5 day。
3. **A（nvme_of fused fabric）** —— 见 [fused plan](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-09-fused-fabric-and-chap-conformance.md)。
4. 之后按需 V9 RDMA / V10 DMA backend（确认愿景对齐 + 硬件/host-root 到位再投）。
