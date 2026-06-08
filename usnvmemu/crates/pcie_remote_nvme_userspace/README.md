# pcie_remote_nvme_userspace — 用户态 NVMe 控制器示例

通过 [pcie_remote 协议](../../specs/2026-05-29-pcie-remote-design.md)
把一个**完全运行在 host 用户态**的 NVMe 2.0 控制器暴露给 OpenHCL VM 中
的 guest（Windows / Linux），让 guest 的 `nvme.sys` / `nvme` 内核驱动
看到并使用一块 "真"虚拟 NVMe 盘。

> **教学项目**：覆盖 NVMe 2.0 协议主路径（Admin + IO + AEN +
> Reservation + Self-Test + FW + SMART log + Multi-NS + Multi-Queue）。
> 优先**正确性** + **注释密度**；性能在性价比合理处优化（删 per-IO
> fsync 走 FLUSH，PRP list 128 KiB MDTS）。

---

## 架构

```
┌─────────────────────── Host Windows ───────────────────────┐
│                                                            │
│   pcie_remote_nvme_userspace.exe                           │
│   (本 example - Rust 用户态进程)                            │
│   ┌─────────────────┐  ┌────────────────────────────┐      │
│   │  NvmeController │←→│ pcie_remote_userspace_sdk  │      │
│   │  · CC/CSTS regs │  │ · transport (vsock/tcp)    │      │
│   │  · SQ/CQ mgmt   │  │ · DMA token routing        │      │
│   │  · PRP DMA      │  │ · MSI-X fire_interrupt     │      │
│   │  · NS HashMap   │  │ · MMIO callbacks           │      │
│   │  · SMART/AEN... │  └────────────────────────────┘      │
│   └─────────────────┘             ↑                        │
│   ↑ backing files                 │ vsock / TCP            │
│   │ (per NSID)                    │                        │
└───┼───────────────────────────────┼────────────────────────┘
    │                               │
┌───▼───────────────────────────────▼────────────────────────┐
│                  Hyper-V VM (Generation 2)                 │
│                                                            │
│   ┌────────────────── VTL2 (OpenHCL Linux) ─────────────┐  │
│   │ pcie_remote_device — kernel-mode shim               │  │
│   │ · vsock handshake / hello                           │  │
│   │ · proxy MMIO/DMA between guest & host               │  │
│   │ · 暴露 emulated PCIe BDF 到 guest                   │  │
│   └─────────────────────────────────────────────────────┘  │
│                            ↑ MMIO / DMA / MSI-X            │
│   ┌────────────────────────▼────────────────────────────┐  │
│   │ VTL0 (Windows / Linux guest)                        │  │
│   │ · nvme.sys 看到 PCI vendor=0x1414 device=NVMe       │  │
│   │ · Get-Disk 见到盘（每 NS 一块）                       │  │
│   │ · partition / format / 文件 IO 全走真 NVMe 路径       │  │
│   └─────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

**关键点**：
- Host 用户态可以**热重启** controller，VTL2 重新 handshake
- DMA 走 vsock 序列化往返；性能上限是 vsock RTT（~50-200 μs）
- 完全无 root：host 进程不需要 kernel module，guest 不需要修改驱动

---

## 当前已实现的 NVMe 2.0 spec 子集

### Admin 命令

| Opcode | 名称 | 状态 |
|--------|------|------|
| 0x01 | Create IO SQ | ✅ |
| 0x05 | Create IO CQ | ✅ |
| 0x00 | Delete IO SQ | ✅ |
| 0x04 | Delete IO CQ | ✅ |
| 0x06 | Identify | ✅ (CNS 0x00/0x01/0x02/0x03/0x06) |
| 0x02 | Get Log Page | ✅ (LID 0x01/0x02/0x03/0x06/0x80) |
| 0x08 | Abort | ✅ (no-op success) |
| 0x09 | Set Features | ✅ (14 标准 fid 真追踪) |
| 0x0a | Get Features | ✅ (回填) |
| 0x0c | Async Event Request | ✅ (真 queue + tick 自动 fire) |
| 0x10 | Firmware Commit | ✅ (4 action + slot 切换) |
| 0x11 | Firmware Image Download | ✅ (DMA-read chunk 累积) |
| 0x14 | Device Self-Test | ✅ (后台 tick 推进 + 完成 fire AEN) |
| 0x15 | Namespace Attachment | ✅ |
| 0x18 | Keep Alive | ✅ |
| 0x80 | Format NVM | ✅ (per-NS + LBAF 0/1 + PI Type 0/1) |
| 0x81/0x82 | Security Send/Receive | ❌ (返 INVALID_OPCODE) |
| 0x84 | Sanitize | ❌ (返 INVALID_FIELD) |
| 0x0d | Namespace Mgmt | ❌ (返 INVALID_FIELD，本 example 静态 NS) |

### NVM IO 命令

| Opcode | 名称 | 状态 |
|--------|------|------|
| 0x00 | Flush | ✅ (per-NS + 0xFFFF_FFFF broadcast) |
| 0x01 | Write | ✅ (3-tier PRP: ≤4K / ≤8K / PRP list 至 128 KiB) |
| 0x02 | Read | ✅ (同上) |
| 0x05 | Compare | ✅ (≤ 4 KiB 真比较；> 4K 占位 success) |
| 0x08 | Write Zeroes | ✅ (分块 4K 写零) |
| 0x09 | Dataset Management (TRIM) | 🟡 (返 success，不真 punch_hole) |
| 0x0c | Verify | 🟡 (LBA 边界校验，不真 ECC) |
| 0x0d | Reservation Register | ✅ |
| 0x0e | Reservation Report | ✅ |
| 0x11 | Reservation Acquire | ✅ |
| 0x15 | Reservation Release | ✅ |
| 0x04 | Write Uncorrectable | ❌ |

### 其它

- **MDTS** = 5 → 单 cmd 最大 128 KiB transfer
- **MSI-X** vectors = 4 (admin + 3 IO)
- **IO Queues** 至多 4 SQ + 4 CQ（per Set Features 0x07 协商）
- **SMART log** 真追踪 host reads/writes/lba_*/power_on_hours/num_err
- **AEN** tick 自动 fire（self-test 完成 / 新 error）
- **Error Info Log** 64-entry ring (ELPE=63)
- **Firmware slot info** AFI / FRS 真序列化（7 slot）
- **Self-Test Log** 564-byte spec layout，反映 in-progress + 上次结果
- **Reservation Status** spec § 6.14 Figure 197 真序列化

### 未实现（按优先级排序）

- ZNS (Zoned Namespace) — 见 [ZNS_DESIGN.md](ZNS_DESIGN.md)
- KV Namespace / Computational Storage
- Endurance Group / NVM Sets
- ANA (Multipath)
- Telemetry / Persistent Event / LBA Status logs
- Power States PSD0..31
- CMB / PMR / Boot Partition
- Directives / Streams
- PI (Protection Information) 完整 CRC/RefTag 引擎
- Compare > 4 KiB 路径
- Multi-host HOSTID (16-byte)

---

## 用法

### 准备 (Windows host)

```powershell
# 1. 创建 backing 文件（每 NS 一个）
fsutil file createnew C:\temp\nvme_ns1.img 1073741824   # 1 GiB
fsutil file createnew C:\temp\nvme_ns2.img 1073741824   # 可选 NS 2

# 2. 构建 OpenHCL with pcie_remote 设备
#    见 docs/superpowers/scripts/hyperv/build_pcie_igvm.sh

# 3. 创建 Hyper-V Gen 2 VM with -GuestStateIsolationType OpenHCL
#    见 docs/superpowers/scripts/hyperv/create_openhcl_vm_correct.ps1

# 4. 跑 controller（多 NS）
pcie_remote_nvme_userspace.exe `
    --vm-id <vm-guid> `
    --port 50000 `
    --backing-file C:\temp\nvme_ns1.img `
    --backing-file C:\temp\nvme_ns2.img
```

### Guest 验证 (Windows)

```powershell
# 看到 NVMe 盘
Get-Disk | Where-Object BusType -eq 'NVMe'

# Initialize + 用 NVMe 真 IO
$d = Get-Disk -BusType NVMe | Select -First 1
Initialize-Disk -Number $d.Number -PartitionStyle GPT
$p = New-Partition -DiskNumber $d.Number -UseMaximumSize -AssignDriveLetter
Format-Volume -DriveLetter $p.DriveLetter -FileSystem NTFS -NewFileSystemLabel NVME

# 写大文件触发 PRP list (>= 8 KiB IO)
fsutil file createnew "$($p.DriveLetter):\big.dat" $((4MB))

# 读 SMART log
Get-StorageReliabilityCounter -PhysicalDisk (Get-PhysicalDisk -DeviceNumber $d.Number)
```

### TCP 模式（非 Windows / 单元测试）

```bash
pcie_remote_nvme_userspace --tcp-addr 127.0.0.1:50000 \
    --backing-file /tmp/ns1.img \
    --backing-file /tmp/ns2.img
```

---

## 代码导览

| 文件 | 内容 |
|------|------|
| [src/main.rs](src/main.rs) | 入口 + clap CLI + reconnect loop |
| [src/cmd.rs](src/cmd.rs) | NVMe Sqe/Cqe wire types + opcode/fid/sc 常量 + IdentifyController/Namespace builder（用 nvme_spec crate 200+ 字段） |
| [src/regs.rs](src/regs.rs) | BAR0 寄存器 layout + CC/CSTS/CAP/AQA 等 bit field |
| [src/controller/mod.rs](src/controller/mod.rs) | NvmeController 主结构 + state machine（enable/disable/reset/tick）+ DMA 完成回调（PRP list / dual / single）+ SMART/AEN/Self-Test/Error log 真追踪 + Reservation 状态机 |
| [src/controller/admin.rs](src/controller/admin.rs) | Admin command dispatch (15+ opcode) |
| [src/controller/io.rs](src/controller/io.rs) | NVM IO command dispatch (Read/Write/Flush/Compare/Reservation/…) |

---

## 写下一个 PCIe device example

本 example 展示了用 `pcie_remote_userspace_sdk` 写一个完整 PCIe 设备
的所有要素。写下一个（例如 xHCI、virtio-net、自定义 FPGA 加速器）的
步骤：

### 1. 新建 binary

```bash
cd docs/superpowers/examples
cp -r pcie_remote_nvme_userspace pcie_remote_<your_device>
```

改 `Cargo.toml` 包名 + 删 NVMe 专用依赖（`nvme_spec` / `storage_string`）。

### 2. 实现 `PcieDevice` trait

```rust
use pcie_remote_userspace_sdk::*;

pub struct MyDevice { /* 私有状态 */ }

impl PcieDevice for MyDevice {
    fn on_mmio_read(&mut self, ctx: &mut DeviceCtx<'_>, offset: u64, size: u32) -> u64 {
        // BAR 读：返寄存器值
    }
    fn on_mmio_write(&mut self, ctx: &mut DeviceCtx<'_>, offset: u64, size: u32, value: u64) {
        // BAR 写：处理 doorbell / control regs；可能 ctx.dma_read/write 触发 DMA
    }
    fn on_dma_complete(&mut self, ctx: &mut DeviceCtx<'_>, token: u64, ok: bool, data: Vec<u8>) {
        // DMA 完成回调，按 token 路由到 pending op
    }
    fn tick(&mut self, ctx: &mut DeviceCtx<'_>) {
        // 后台周期工作（heartbeat / 状态推进 / interrupt coalescing）
    }
    fn reset(&mut self, kind: u32) { /* PCIe 重置 */ }
}
```

### 3. 关键 SDK API

- `ctx.dma_read(gpa, bytes)` → 返 token；完成后 `on_dma_complete(token, ok=true, data)`
- `ctx.dma_write(gpa, data)` → 返 token；完成后 `on_dma_complete(token, ok=true, data=空)`
- `ctx.fire_interrupt(vector)` → 触发 MSI-X
- SDK transport 自动 reconnect + tick interval 配置（参 `RunOptions`）

### 4. 必读

- [SDK source](../../../../vm/devices/pcie_remote_userspace_sdk/src/)
- [pcie_remote protocol spec](../../specs/2026-05-29-pcie-remote-design.md)
- [pcie_remote_device VTL2 shim](../../../../vm/devices/pcie_remote_device/src/)

---

## 测试

```bash
cd usnvmemu/crates/pcie_remote_nvme_userspace
cargo test          # 11+ 单测
cargo clippy --tests -- -D warnings  # 干净
```

WSL 交叉编译 Windows exe：

```bash
# 用 build_support/windows_cross 工具链
bash docs/superpowers/scripts/build-windows-cross.sh
```

---

## 设计哲学

> "用户的核心目标是教学研究，因此功能完整性、注释度等等也很重要，但
> 不应该故意舍弃性能，除非性能性价比（而非其他性价比）过犹不及。"

- **注释引 spec § 章节** — 每个常量 / 字段 / 状态都告诉读者去 spec 哪
  里查
- **commit message 详细动机** — Phase A→B→C→…→H7 每步都解释为什么这
  样做、哪些 reviewer 反馈、性能权衡
- **rust-reviewer subagent 每 Phase 跑一次** — 已发现 2 个 CRITICAL
  数据损坏 + 多个 HIGH spec 违规并修复
- **单测 byte-layout 校验** — 关键 spec 结构（IdentifyController /
  SMART / Self-Test / FW Slot Info / Error Info Log / Reservation）每
  个 offset 都断言

---

## 历史 Phase

完整开发时间线见 [SESSION_LOG.md](../../PCIE_REMOTE_SESSION_LOG.md)：

- **Phase A** — Identify 用 nvme_spec NVMe 2.0c 完整字段
- **Phase B+C+D** — Admin opcodes + Get Log Page + IO opcodes
- **Phase E** — PRP list 支持 (> 2 page IO)
- **Phase F** — 真 SMART log + AEN 队列
- **Phase G** — Self-Test 状态机 + AEN 自动触发 + Error Log 累积
- **Phase H1** — Set/Get Features 真追踪
- **Phase H2** — Multiple IO Queues (默认 4)
- **Phase H3** — Compare 真 DMA-read 比较
- **Phase H4** — Multiple namespaces
- **Phase H5** — Firmware download/commit/activate 状态机
- **Phase H6** — Reservation Register/Acquire/Release/Report
- **Phase H7** — Protection Information capability + LBAF[1]
- **Phase J** — Reservation 多 host model (HOSTID 16B + RKEY 8B)
- **Phase K1** — T10 DIF CRC16 engine（spec § 8.3，table-free 教学版）
- **Phase K2** — Compare dual-PRP + PRP list 路径（与 Write 三档分流对齐）
- **Phase K3** — NS Management Create/Delete + 临时 backing
- **Phase K4a/b** — 真 PI Write/Read/Verify/WriteZeroes（单 LBA，
  block_bytes=4104 interleave，positional IO 防 cursor race）
- **Phase K4c** — 多 LBA PI Write/Read 真实现（dual-PRP ≤ 2 page，
  PiWriteAccum 累积器 + per-LBA verify）
- **Phase K4c-list** — PI Write/Read PRP-list 路径（> 2 page，list fetch
  + per-entry data DMA）
- **Phase K5** — Sanitize 5 action state machine + IO quiesce
- **Phase K6** — Doorbell Buffer Config（fast doorbell skip vsock round-trip）
- **Phase K9** — Reservation full HOSTID 16-byte + monotonic GEN
- **Phase L1** — Zoned Namespace 基础（per-NS opt-in via `--zns-nsid`，
  Zone Mgmt Send/Receive/Append + 完整 state machine）
- **Phase L1c** — Identify NS CNS 0x05 (ZNS) 上报 MAR/MOR/ZSZE
  （0-based + 0xFFFFFFFF=unlimited 翻译正确）
- **Phase L1d** — Identify CNS 0x06 (Specific Controller-for-CSI) +
  CNS 0x08 (I/O CS Independent NS Identify) + CNS 0x1c (I/O Command Sets
  bitmap = NVM|ZNS)
- **Phase L1f** — PI + ZNS 组合（plain WRITE PI 完成路径加 advance_zns_wp，
  ZONE_APPEND 因 SECTOR_SIZE 常量限制仍 reject）
- **Phase L2** — Reservation Notification Log (0x80)
- **Phase L4** — Directive Send/Receive (Streams)
- **Phase L5** — Security Send/Receive + Virtualization Mgmt + Get LBA Status
- **Phase M1** — Set Features Interrupt Coalescing（cdw11 解析）
- **Phase M1b** — 真 Interrupt Coalescing batch + tick-driven time flush
  （spec § 5.21.1.8，should_fire_irq 纯函数 + 单测）
- **Phase M-2** — plain NVM_WRITE / WRITE_ZEROES / READ 在 ZNS NS 上强制
  SWR + zone state + 边界 + WP 推进（reviewer 修复）
- **Phase M2** — mmap zero-copy backing（Namespace.mmap +
  Unix mmap/Windows CreateFileMapping，hot-path 抹掉 kernel buf 拷贝）
- **Phase M3** — per-queue parallel dispatch（架构决策记录 M3_PARALLEL_DESIGN.md：
  当前 single-thread async 是 SDK 一次 &mut PcieDevice 的结果；真多线程
  需 SDK + per-queue actor 重写。教学版保留 single-thread 换 0 race/lock
  风险）
- **Phase N1** — SDK DeviceCtx outbound 单测
- **Phase N1b** — SDK dispatch_inbound 路由测试 + CaptureDevice fixture
- **Phase N2** — NVMe 12-step 生命周期教学文档（docs/NVME_LIFECYCLE.md）
- **Phase O1** — Simple Copy (0x19) — controller-side data movement
- **Phase O2** — Fused Compare-and-Write dispatcher 检测
- **Phase O3** — Fused C+W 真 atomic chain（NvmCompareSinglePrpFused：
  Compare pass → 真 dispatch Write；Compare fail → Write 也收 COMPARE_FAILURE
  不写盘。IdentifyController.fuses bit 0 = 1 真 advertise）
- **Phase P1** — Endurance Group (CNS 0x19) + NVM Set (CNS 0x04) +
  Log Page 0x09 真字段填充 + PTPL (Persist Through Power Loss) sidecar
  让 Reservation 跨重启存活
- **H-3** — on_dma_complete 1614 行单方法拆到 controller/completion.rs，
  mod.rs 从 3272 → ~2050 行

每 Phase 都过 rust-reviewer + 多数有 CRITICAL/HIGH 修复。**12 轮 reviewer
后 0 CRITICAL / 0 HIGH / 0 MEDIUM**：当前总单测 **NVMe controller 46 +
SDK 13 = 59**。`cargo clippy --all-targets -- -D warnings` 全绿。
WSL → Windows MSVC 交叉编译验证可生成 5.4 MiB PE32+ exe。
Production-ready as teaching example。

## Phase Q 系列追加（NVMe 2.0 spec coverage 完整化）

- **Q1** PRACT=1 真处理（PI NS 上 driver 用 PRACT=1 走 controller 自动
  generate/strip；PRACT=0 on PI NS 拒绝因不支持 driver-supplied inline
  tuple；PRACT=1 on non-PI NS 拒绝因无 PI 上下文）
- **Q2** ZONE_APPEND on PI NS 解禁（per-LBA PI tuple compute + interleave
  4104-byte block 写 backing；与 plain WRITE PI 行为一致）
- **Q3** Telemetry Log 0x07/0x08 真填充（header + Data Area 1 含 host
  I/O counters snapshot 让 `nvme telemetry-log` 能拿到真诊断）
- **Q4** ANA Identify Controller advertise（CMIC bit 3 ANAR + ANACAP 0x0F
  + ANAGRPMAX/NANAGRPID=1 让 `nvme ana-show` 能 enumerate）
- **Q5** Boot Partition register set (BPRSEL/BPMBL RW，BPINFO=0 不
  advertise；完整 BP image 服务留 future work)
- **Q7** Lockdown command 0x24 (NVMe 2.0 § 5.18) 真 enforce admin opcode
  禁用/启用 + SC 0x23 COMMAND_PROHIBITED_BY_LOCKDOWN
- **Q8** Format SES=2 Cryptographic Erase: crypto_gen counter 暴露到
  SMART vendor-specific byte 232..236 让 driver 感知 key 销毁
- **Q9** Reservation Notification Mask 0x82 + Reservation Persistence 0x83
  通过 features map 自动 Set/Get
- **Q10** SDK DeviceCtx::for_testing infrastructure，让外部 crate 单测
  可注入 mock buffer 验 protocol invariant（doc-hidden 避免 production
  API leak）
- **Q11** mod.rs 拆 enable.rs + mmio.rs（2102 → 1885 行，单一职责）
- **Q12** WSL → Windows MSVC 交叉编译脚本支持 standalone example
  (exclude 列表) — 一行命令 build PE32+ exe

## Phase R 系列 (SGL 完整化 + 公开 PCI BAR 上报)

- **R1** SGL Data Block descriptor 真处理 (CDW0 bit24=1 = SGL data mode)
- **R3** Identify Controller `sgls` 字段 advertise SGL 支持 (NVMe 2.0 §
  5.17.2.1 Figure 277)
- **R4** RBAR (Read BAR) ADR field 上报真实 PCI register layout

commit `f19c193f`。

## Phase S 系列 (NS Attach / Controller List / NS Write Protect / ANA state machine)

- **S1** Namespace Write Protection (Feature 0x84) + SC 0x20 ATTEMPTED_WRITE_TO_RO_RANGE
- **S2** COPY conflict detect (Simple Copy spans 跟 reservation 冲突 → SC 0x83)
- **S3** Identify NS 加 NAWUN/NAWUPF/NACWU/NOIOB/NPWG/NPWA/NPDG/NPDA 真填
  (driver 用来决定 IO alignment / prefer write granularity)
- **S4** Namespace Attachment (admin cmd 0x15) 真 Attach/Detach state +
  multi-controller share 假设
- **S5** Controller List CNS 0x12/0x13 (NVMe 2.0 § 5.17.2.16-17)
- **S6** Reservation Notification Log records resv events (Log Page 0x80
  按 spec 序列化)
- **S7** ANA (Asymmetric Namespace Access) state machine + Change AEN
  (spec § 5.20 ANAGRP state transitions)

commits `d0b36b19`..`f8d847ea` + reviewer round 1 batch `10f987f5`。

## 状态总览 (2026-06-06)

本 crate 已实现 NVMe 2.0 spec 的 admin + IO 大部分公开 feature；剩余主要
deferred 的是 **真 ZNS 完整** (ZNS_DESIGN.md 标 deferred ROI 不对) + **多
queue parallel dispatch** (M3_PARALLEL_DESIGN.md ADR)。具体 spec coverage
+ test count + reviewer round 详 `README.md` 顶 + Phase 列表。

下游消费者：
- [`pcie_remote_userspace_sdk`](../pcie_remote_userspace_sdk/) — SDK 把
  本 controller 跑在 vsock/TCP 之上
- [`pcie_remote_rng_userspace`](../pcie_remote_rng_userspace/) — 第二个
  PcieDevice 教学 example
- **`nvme_of_tcp_target`** — 把本 controller 包成 NVMe-oF TCP target；
  详 [`../nvme_of_tcp_target/README.md`](../nvme_of_tcp_target/README.md)
- **vfio-user via `pcie_vfio_user_sdk`** — 把本 controller 通过 vfio-user
  UNIX socket 挂给 QEMU
