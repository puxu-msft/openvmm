# nvme_firmware — 用户态 NVMe 控制器 firmware（runtime-agnostic core + 三接入）

一份**完全运行在用户态**的 NVMe 2.0 控制器 firmware。controller 核心
runtime-agnostic，只认一个 `trait Transport`（[`pcie_device_core`](/usnvmemu/crates/pcie_device_core/)
domain core），同一份 controller 通过三条标准接入暴露 NVMe 盘给上游：

| 接入 | wire | adapter crate | 验证 |
|------|------|---------------|------|
| **OpenHCL VTL2** | vsock + PCIe Remote | `pcie_device_sdk`（`--features openhcl`） | 真 Hyper-V guest 出盘 |
| **OpenVMM / dev** | TCP + PCIe Remote | `pcie_device_sdk`（`--tcp-addr`） | 跨进程 e2e |
| **QEMU / Cloud Hypervisor / SPDK** | vfio-user UNIX socket | `vfio_user_transport`（`--features vfio-user`） | 真 QEMU guest 出盘 |
| **Linux/Windows NVMe initiator**（隔 NVMe-oF 抽象层） | NVMe-oF TCP wire | [`nvme_of_tcp_target`](/usnvmemu/crates/nvme_of_tcp_target/) | 真 nvme-cli / WS2025 出盘 |

> **教学到生产平滑过渡**：覆盖 NVMe 2.0 协议主路径（Admin + IO + AEN + Reservation +
> Self-Test + FW + SMART/Telemetry log + Multi-NS/Queue + PI + SGL + ZNS + CMB）。优先
> **正确性 + 注释密度**；性能在性价比合理处优化（FLUSH 替 per-IO fsync、零拷贝 mmap
> backing、CMB map 模式零拷贝）。完整愿景见 [PROJECT_VISION.md](/usnvmemu/docs/PROJECT_VISION.md)。

---

## 架构（六边形：domain core + trait Transport + adapters）

```
┌───────────────────────── 用户态进程（host）─────────────────────────┐
│                                                                     │
│   nvme_firmware                                                     │
│   ┌──────────────────────────┐    ┌──────────────────────────────┐ │
│   │  NvmeController (core)    │    │  pcie_device_core            │ │
│   │  · CC/CSTS/CAP/CMB regs   │    │  · trait Transport (3 原语)  │ │
│   │  · SQ/CQ + doorbell       │←──→│  · DeviceCtx (dma + irq)     │ │
│   │  · PRP/SGL DMA + PI 引擎   │    │  · 零依赖 domain core        │ │
│   │  · NS HashMap + backing   │    └──────────────┬───────────────┘ │
│   │  · SMART/AEN/Self-Test... │       feature-gated adapter         │
│   └──────────────────────────┘    ┌──────────────┴───────────────┐ │
│   ↑ backing 文件 / mmap            │ pcie_device_sdk (vsock/TCP)  │ │
│   │ (per NSID)                     │ vfio_user_transport (socket) │ │
│   │                                │ nvme_of_tcp_target (NVMe-oF) │ │
└───┼────────────────────────────────┴──────────────┬───────────────┘ │
    │                                                │ vsock / TCP /
    │                                                │ vfio-user / NVMe-oF
┌───▼────────────────────────────────────────────────▼─────────────────┐
│  上游 hypervisor / initiator                                          │
│  · OpenHCL VTL2 paravisor（pcie_remote_device shim）→ Hyper-V guest   │
│  · QEMU vfio-user-pci → guest                                         │
│  · Linux nvme-cli / Windows nvme（NVMe-oF TCP）                       │
│  guest 的 nvme.sys / nvme 内核驱动看到一块"真"虚拟 NVMe 盘            │
└───────────────────────────────────────────────────────────────────────┘
```

**关键点**：
- **controller 核心 runtime-agnostic** — 只 `use std/zerocopy/pcie_device_core`；换 transport
  不改 controller，换 runtime 只改 bin（架构原则见 [PROJECT_VISION §7](/usnvmemu/docs/PROJECT_VISION.md)）。
- **wire 协议层属 transport，不属 firmware** — PCIe Remote / vfio-user / NVMe-oF 都在 adapter crate。
- **无 root**：host 进程不需 kernel module，guest 不需改驱动；controller 可热重启、对端重 handshake。
- 加第 5 条 transport 的教程见 [HOW_TO_ADD_TRANSPORT.md](/usnvmemu/docs/HOW_TO_ADD_TRANSPORT.md)。

---

## 实现状态（spec coverage）

**权威单一事实源 = [docs/SPEC_CONFORMANCE.md](docs/SPEC_CONFORMANCE.md)**（每命令/特性逐行
✅/◐/⊘/⏸/✗ + 代码锚点 + 缺什么）。本节只给高层概览，**不复制表**（避免两处漂移）。

**已实现主路径**：
- **Admin**（`controller/admin.rs`，25+ opcode）：Identify（CNS 0/1/2/3/4/5/6/8/12/13/19）、
  Get/Set Features、Get Log Page、Create/Delete IO SQ/CQ、Abort（真定位中止）、Format NVM
  （LBAF 0/1/2 + PI Type 0/1）、FW Download/Commit、Device Self-Test、**NS Management**（create/delete）、
  NS Attachment、**Sanitize**（状态机 + AEN，backing 不真擦 ⊘）、Doorbell Buffer Config、Lockdown、
  AER（真队列 + tick fire）、Security Receive ◐（仅 SECP=0 Protocol List）、Directive Send/Receive ⊘（stub）。
- **NVM IO**（`controller/io.rs`）：Read/Write（3-tier PRP + **全形态 PI** + **SGL**）、Compare
  （+ Fused Compare-and-Write 原子）、Flush、Write Zeroes、DSM/TRIM、Copy、Verify、Zone
  Mgmt Send/Receive/Append（ZNS）、Reservation Register/Acquire/Release/Report。
- **数据路径**：PRP 单/双/list（≤2 MiB）、**SGL**（segment 链 + Bit Bucket + CMB-relative）、
  **Protection Information** 全形态（inline/separate × PRACT 0/1 × 全 nlb；**SGL×PI** P-B/P-C/E1
  metadata-SGL）、**CMB**（双模 trap/map）、MDTS 强制、DBBUF shadow doorbell、Boot Partition Read。
- **可观测**：SMART / Error Info / FW Slot / Self-Test / Telemetry / ANA / Reservation-Notif /
  Sanitize / Discovery（NVMe-oF）log；AEN 事件源（self-test/sanitize done、error、reservation、ANA change）。

**诚实拒绝的 out-of-scope**（教学 firmware 不半吊子）：Security Send（TCG OPAL）、Virtualization
Mgmt（SR-IOV）、Write Uncorrectable（backing 无 ECC）、真 crypto-erase。详见 SPEC_CONFORMANCE §4。

---

## 用法

backing 文件按 NSID 给（每 `--backing-file` 一块 NS）。三接入选其一：

### OpenHCL VTL2（vsock，默认 feature）

```bash
nvme_firmware --vm-id <vm-guid> --port 50000 \
    --backing-file /path/ns1.img --backing-file /path/ns2.img
```
OpenHCL IGVM 构建 / Hyper-V VM 创建 / 真机出盘见 [RUNBOOK_HOST_ROOT.md](/usnvmemu/docs/RUNBOOK_HOST_ROOT.md)
+ [VFIO_USER_IN_UNDERHILL.md](/usnvmemu/docs/VFIO_USER_IN_UNDERHILL.md)（VTL2-内 vfio-user 那条线）。

### TCP（OpenVMM / 单元测试 / 非 Windows）

```bash
nvme_firmware --tcp-addr 127.0.0.1:50000 --backing-file /tmp/ns1.img
```

### vfio-user（QEMU / Cloud Hypervisor / SPDK）

```bash
nvme_firmware --vfio-user-sock /tmp/nvme.sock --backing-file /tmp/ns1.img
# QEMU: -device vfio-user-pci,socket=/tmp/nvme.sock
```

### NVMe-oF TCP target

包成 NVMe-oF target（Linux nvme-cli / Windows initiator 直连）见
[`nvme_of_tcp_target/README.md`](/usnvmemu/crates/nvme_of_tcp_target/README.md)
+ 导览 [NVME_OF_TCP.md](/usnvmemu/docs/NVME_OF_TCP.md)。

### 常用 CLI flag（`--help` 全量）

| flag | 作用 |
|------|------|
| `--backing-file <f>`（可多次） | 每文件一块 NS backing |
| `--zns-nsid <n,...>` | 指定 NS 走 ZNS（Zoned Namespace） |
| `--not-ready-nsid <n,...>` | NS 起始 not-ready（待 Format 转 ready） |
| `--boot-partition-file <f>` | 装只读 Boot Partition 出厂镜像 |
| `--cmb-mode off\|trap\|map` + `--cmb-size`/`--cmb-bir` | Controller Memory Buffer 双模 |
| `--max-queue-entries` / `--io-queue-pairs` / `--max-namespaces` | 容量上限 |
| `--vid` / `--ssvid` | PCI Vendor ID / Subsystem Vendor ID（默认 0x1414 / 0x0） |

### Guest 验证（Windows）

```powershell
Get-Disk | Where BusType -eq 'NVMe'                       # 看到盘
$d = Get-Disk -BusType NVMe | Select -First 1
Initialize-Disk $d.Number -PartitionStyle GPT
$p = New-Partition $d.Number -UseMaximumSize -AssignDriveLetter
Format-Volume -DriveLetter $p.DriveLetter -FileSystem NTFS
fsutil file createnew "$($p.DriveLetter):\big.dat" $((4MB))  # 触发 PRP list
```

---

## 代码导览

| 文件 | 内容 |
|------|------|
| [src/main.rs](src/main.rs) | bin 入口 + clap CLI + transport 选择（openhcl vsock / TCP / vfio-user）+ reconnect loop |
| [src/lib.rs](src/lib.rs) | crate 公开 API + 容量常量 |
| [src/cmd.rs](src/cmd.rs) | NVMe Sqe/Cqe wire types + opcode/fid/sc 常量 + Identify Controller/Namespace builder（经 `nvme_spec`） |
| [src/regs.rs](src/regs.rs) | BAR0 寄存器 layout（CC/CSTS/CAP/AQA/CMB* bit field） |
| [src/pi.rs](src/pi.rs) | Protection Information 引擎（T10 DIF CRC16 + Ref/App Tag，spec § 5.2 教学版 table-free） |
| [src/sgl.rs](src/sgl.rs) | SGL descriptor parse + sub_type classifier + CMB-relative rebase（`resolve_sgl_address`） |
| [src/controller/mod.rs](src/controller/mod.rs) | `NvmeController` 主结构 + state machine + `PendingOp`/累积器 + SMART/AEN/Self-Test/Reservation 状态 |
| [src/controller/admin.rs](src/controller/admin.rs) | Admin command dispatch（25+ opcode） |
| [src/controller/io.rs](src/controller/io.rs) | NVM IO dispatch（Read/Write/Compare/Flush/DSM/Copy/Verify/Zone/Reservation）+ PRP/SGL/PI 分流 |
| [src/controller/completion.rs](src/controller/completion.rs) | `on_dma_complete` 回调路由（PRP-list / SGL walk + scatter/gather / PI finalize） |
| [src/controller/cmb.rs](src/controller/cmb.rs) | Controller Memory Buffer 双模（trap 转发 / map memfd 零拷贝） |
| [src/controller/enable.rs](src/controller/enable.rs) | CC/CSTS enable/disable/reset/shutdown 生命周期 + CSTS.CFS |
| [src/controller/mmio.rs](src/controller/mmio.rs) | BAR0 MMIO read/write 分发（doorbell / control regs / CMB / Boot Partition） |
| [src/controller/prp.rs](src/controller/prp.rs) | PRP 页内偏移感知分段（single/dual/list tier，#4 非页对齐布局） |
| [src/controller/logs.rs](src/controller/logs.rs) | Get Log Page 序列化器（SMART/Error/FW/Self-test/Telemetry/ANA/... 纯函数 builder） |
| [src/controller/discovery_log.rs](src/controller/discovery_log.rs) | Discovery Log Page（LID 0x70，NVMe-oF Discovery controller 用） |
| [src/controller/reservation.rs](src/controller/reservation.rs) | Reservation 状态机（Register/Acquire/Preempt/Release + PTPL sidecar） |
| [src/controller/tests.rs](src/controller/tests.rs) | controller lib 单测（byte-layout + 数据路径差分 oracle） |

---

## 测试

```bash
cd usnvmemu/crates/nvme_firmware
cargo test --lib                       # 260 controller 单测
cargo test --tests                     # 集成 e2e（openhcl / vfio-user cmb·guest·wire / regression）
cargo clippy --all-targets -- -D warnings
```

- **测试金字塔**：lib 单测（controller）→ 集成 e2e（transport + firmware 跨进程）→ Python harness
  （跨进程 wire 实证，见各 transport crate）→ 真 host（Linux nvme-cli / QEMU / Hyper-V）。
- 测试**覆盖率/质量**（"测试有没有牙"）见 [docs/TEST_COVERAGE_PROGRESS.md](docs/TEST_COVERAGE_PROGRESS.md)
  + [docs/TEST_QUALITY.md](docs/TEST_QUALITY.md)；coverage-guided fuzz 见 [docs/FUZZING.md](/usnvmemu/docs/FUZZING.md)。
- WSL → Windows MSVC 交叉编译生成 PE32+ exe：用 [`build_support/windows_cross`](/build_support/windows_cross/) 工具链。

---

## 设计哲学

> "核心目标是教学研究 —— 功能完整性、注释度很重要，但不刻意舍弃性能，除非性价比过犹不及。"

- **注释引 spec § 章节** — 每个常量/字段/状态都指向 spec 出处。
- **rust-reviewer subagent 每语义单元跑一次** — 历史发现并修复多个 CRITICAL 数据损坏 + HIGH spec 违规。
- **byte-layout 单测** — 关键 spec 结构（Identify / SMART / Self-Test / FW Slot / Error Log /
  Reservation）每个 offset 都断言；数据路径用差分 oracle（backing/compute 期望 + 显式断每条 DMA）。
- **教学/生产边界不模糊** — 每个 stub/简化都在代码注释 + SPEC_CONFORMANCE 显式标注（⊘/✗），诚实拒绝优于半吊子。

---

## 文档地图

| 文档 | 内容 |
|------|------|
| [docs/SPEC_CONFORMANCE.md](docs/SPEC_CONFORMANCE.md) | **权威**命令/特性实现状态表（逐行 + 锚点） |
| [docs/NVME_LIFECYCLE.md](docs/NVME_LIFECYCLE.md) | NVMe 12-step 生命周期教学 |
| [ZNS_DESIGN.md](ZNS_DESIGN.md) / [M3_PARALLEL_DESIGN.md](M3_PARALLEL_DESIGN.md) | ZNS 设计 / 多线程 dispatch ADR |
| [/usnvmemu/docs/MILESTONES.md](/usnvmemu/docs/MILESTONES.md) | **历史里程碑档**（Phase A→S + SGL + PI + W vfio-user + V NVMe-oF 全时间线） |
| [/usnvmemu/docs/PROJECT_VISION.md](/usnvmemu/docs/PROJECT_VISION.md) · [ROADMAP.md](/usnvmemu/docs/ROADMAP.md) · [DECISIONS.md](/usnvmemu/docs/DECISIONS.md) · [LESSONS.md](/usnvmemu/docs/LESSONS.md) | 愿景 / 待办 / ADR / 教训 |

**下游消费者 / 相关 crate**：
- [`pcie_device_core`](/usnvmemu/crates/pcie_device_core/) — 零依赖 domain core（`trait Transport` + `DeviceCtx`）
- [`pcie_device_sdk`](/usnvmemu/crates/pcie_device_sdk/) — OpenHCL vsock / OpenVMM TCP adapter
- [`vfio_user_transport`](/usnvmemu/crates/vfio_user_transport/) — vfio-user UNIX socket adapter
- [`nvme_of_tcp_target`](/usnvmemu/crates/nvme_of_tcp_target/) — 把本 controller 包成 NVMe-oF TCP target
- [`rng_device_example`](/usnvmemu/crates/rng_device_example/) — 第二个 `pcie_device_core` 教学 example
