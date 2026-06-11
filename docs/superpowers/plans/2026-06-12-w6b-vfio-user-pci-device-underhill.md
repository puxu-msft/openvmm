# W6b — vfio-user NVMe ChipsetDevice 集成进 underhill（完整功能设备）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development 或 superpowers:executing-plans 逐 task 实现。步骤用 checkbox（`- [ ]`）。

**Goal:** 把 W6a 的 async `VfioUserClient` 集成进真 `underhill_core`，做一个 **guest 能枚举并驱动**的 vfio-user NVMe PCIe 设备：guest `lspci` 看到设备 → driver attach → 读 BAR0 寄存器 → 建队列 → **跑真 IO**（经 DMA 零拷贝）→ MSI-X 中断送达 guest。

**Architecture:** 照抄成熟模板 `vm/devices/pcie_remote_device`（ChipsetDevice shim + async worker + resolver + boot spawner），**换掉 wire 层**：protobuf 帧（`ToHost`/`codec`/`MmioAccess`/`ReadGpa`）→ W6a `VfioUserClient` 的 typed async 方法（`region_read`/`region_write`/`dma_map`/`set_irqs`）。cfg 枚举 100% 本地（`ConfigSpaceType0Emulator`/`MsixEmulator`）；BAR0 MMIO → defer→worker→`region_read/write`；MSI-X → realize 时建 eventfd 经 `set_irqs` 给 firmware，worker eventfd-wait task → `Interrupt::deliver` 注入 VTL0；DMA → realize 时把 guest RAM fd 经 `dma_map` 传 firmware（零拷贝，非 ReadGpa 消息）。

**Tech Stack:** Rust 2024 / pci_core（ConfigSpaceType0Emulator/MsixEmulator/DeviceBars）/ chipset_device（ChipsetDevice/PciConfigSpace/MmioIntercept/IoResult::Defer）/ vmcore::interrupt（Interrupt::deliver）/ pal_async / mesh / W6a `vfio_user_device` + `vfio_user_wire` / underhill_core 集成 / IGVM（`cargo xflowey build-igvm x64`）+ 真 Hyper-V VM。

---

## 关键约束（执行者必读）

1. **完整有意义的改动，不为小而小**（用户 2026-06-12 铁律，[[meaningful-complete-not-minimal]]）：W6b 目标是设备**真能用**（guest 枚举 + driver 驱动 + 真 IO），不是"出现在 lspci 就算完"。基础→高级分阶段 OK，但都朝"能用"推进。IGVM 慢循环 / 真 VM 迭代成本**不是**砍 scope 的理由（[[user-values-long-term-correctness-not-roi]]）。
2. **承重假设先 POC**（[[poc-before-settling-design]]）：W5a 只证了 *test client*（VTL2 进程）能 open mshv_vtl_low + dma_map 给 firmware。W6b 要 **underhill_core 做中介**把 guest RAM fd 传 firmware——这条 underhill→firmware DMA fd-passing 路径**未验证**。Phase 0 先 POC，再进 DMA 集成。
3. **commit 粒度**（[[commit-granularity-coarse-not-per-step]]）：按语义单元 coarse commit，不逐 task。建议分 3-4 个 coarse commit（新 crate 基础 / underhill 集成 / DMA+IO / 真 VM 验证修复），不逐 step。
4. **多会话共享树**（[[git-commit-shared-index-multisession]]）：`git commit -- <pathspec>` 限定路径，绝不 `git add -A`；不碰 `usnvmemu/docs/DECISIONS.md`（另一会话）。
5. **boot 风险控制**：新 ChipsetDevice 集成错误会让 underhill 起不来 / VM 不 boot。**env-gated（opt-in）+ CVM-gated-off + AbsentPcieDevice fallback + 有界 handshake timeout**（照抄模板的防御）。先 realize-only 确认 boot 不挂，再加 cfg，再加功能。
6. **standalone-first**：所有逻辑（cfg/identity/worker/MMIO 翻译/eventfd→deliver）先在 WSL 用 loopback 单测（对接真 `vfio_user_transport` server）覆盖；IGVM+真 VM 只留给"集成接缝 + guest 真枚举/驱动/IO"的验证。
7. **新增内容用中文**（[[language-chinese-for-changes]]）；原仓库英文不动。

---

## 模板对照表（W6b ← pcie_remote_device 的逐文件改法）

| 模板文件 | W6b 新文件 | 关键改动（wire 层替换） |
|---|---|---|
| `device.rs`（PcieRemoteDevice shim） | `vfio_user_pci_device/src/device.rs` | struct 持 `to_worker: Sender<DeviceRequest>` 不变；cfg/msix 本地不变；MMIO read defer→worker 不变；**MMIO frame 从 `ToHost{MmioAccess}` 改为 `DeviceRequest{ kind: Read/Write, bar, offset, size }`**（无 protobuf） |
| `worker.rs`（Worker 持 BoxedTransport + codec，**全双工** + in_flight map） | `vfio_user_pci_device/src/worker.rs` | **worker 持 vfio_user_device 的全双工 split（writer + reader）+ in_flight: HashMap<msg_id, DeferredRead>**（照抄模板全双工形态，**非**串行 client）。`select_biased!`：from_device arm → 分配 msg_id + 插 in_flight + writer 发 region_read/write 帧（**立即返回，不等 reply**）；inbound arm → reader 读 reply 帧 → 按 msg_id 匹配 in_flight → `token.complete`。MMIO write fire-and-forget（device 立即 Ok，worker 发帧不插 in_flight）。**删 ReadGpa/WriteGpa/DmaCompletion**（DMA 走 dma_map 零拷贝）。**加 eventfd-wait tasks → `interrupts[i].deliver()`**（独立 task，不碰 socket）。单次 wire Err → state Lost（比模板 MAX_BAD_FRAMES 更严格，有意）。 |
| `handshake_spawn.rs`（accept+protobuf handshake） | `vfio_user_pci_device/src/spawn.rs` | **client 主动 connect**（非 listener accept）：`VfioUserClient::connect(driver, unix_path).await` + `handshake()` + identity reads（`get_device_info`/`region_read(CONFIG)`/`get_region_info(BAR0)`/`get_irq_info(MSIX)`）→ 存 `PreparedVfioUserDevice{ client, hardware_ids, bar0_size, msix_count }` 进 prepared map |
| `resolver.rs`（assemble_device） | `vfio_user_pci_device/src/resolver.rs` | 三件套装配（MsixEmulator/DeviceBars/ConfigSpaceType0Emulator）**几乎照抄**；identity 从 prepared 的 `hardware_ids` 取（非 protobuf DeviceDescribe）；**realize 时：建 N 个 eventfd → `client.set_irqs(MSIX, 0, &eventfds)` + `client.dma_map(guest_ram_fd)`**（Phase 0 POC 后）；worker 持 client + eventfds + interrupts |
| `prepared.rs`（PreparedPcieRemoteDevice） | `vfio_user_pci_device/src/prepared.rs` | `PreparedVfioUserDevice{ client: VfioUserClient, hardware_ids, bar0_size, msix_count }`（无 BoxedTransport——client 自持 socket） |
| `state.rs`/`lib.rs` | 同名 | state（Connecting/Live/Lost）+ AbsentPcieDevice 照抄。**删 deadman.rs**（M-3）：模板的 dead-man timer 由 worker per-request 的 wire Err→Lost + （DMA fallback 时）timeout 取代；单次 region_read/write Err 即 Lost（比模板 MAX_BAD_FRAMES 更严格，有意更安全）。 |
| `pcie_remote_resources/src/lib.rs`（handle） | `vfio_user_pci_resources/src/lib.rs` | `VfioUserNvmeHandle{ instance_id, unix_path }` + `impl ResourceId<PciDeviceHandleKind> ID="vfio_user_nvme"` |

> **不复用模板代码**（W0 audit：其 wire 层与 protobuf/vsock 死耦合），但**照抄其结构 + 防御 pattern**（defer/fire-and-forget write/AbsentPcieDevice/state machine/bad-frame→Lost）。

---

## File Structure

新建 crate（**workspace member**，因依赖 pci_core/chipset_device/vmcore 等 workspace crate；这些本就是 underhill 体系内的——与 vfio_user_device 的 exclude 不同。W6b 的设备 crate 是 underhill 内部件，进 workspace）：
- `vm/devices/pci/vfio_user_pci_device/`（device.rs / worker.rs / resolver.rs / spawn.rs / prepared.rs / state.rs / lib.rs / Cargo.toml）
- `vm/devices/pci/vfio_user_pci_resources/`（lib.rs / Cargo.toml — handle 类型）

> **注意**：`vfio_user_device`（W6a client）+ `vfio_user_wire` 是 workspace-**exclude** crate（standalone）。W6b 的设备 crate 是 workspace member，要 path-dep 它们。验证：member crate path-dep exclude crate 是否可行（Phase 1 task 0 先验；exclude crate 只是不被 workspace 自动包含，path-dep 仍可指向）。

改 underhill_core：
- `openhcl/underhill_core/src/options.rs`（新 CLI env `OPENHCL_VFIO_USER_NVME`）
- `openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs`（push `UhVpciDeviceConfig` with `VfioUserNvmeHandle`）
- `openhcl/underhill_core/src/worker.rs`（`add_async_resolver` + connect spawner + prepared map / worker tasks Arc）
- `openhcl/underhill_core/Cargo.toml`（dep 新 crate）

复用部署 harness：`usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/`（改 IGVM 来源 + firmware 启 vfio-user server）。

---

## Phase 0 — 承重 POC：underhill→firmware guest RAM fd-passing（必须先做）

**目的**：验证 underhill_core（VTL2 进程）能拿到 guest RAM 的 fd 并经 SCM_RIGHTS 传给 firmware，firmware mmap @ file_offset=GPA 零拷贝访问。W5a 只证了 test client 自开 mshv_vtl_low；W6b 要 underhill 的 `GuestMemory`（resolver params 给的 `params.guest_memory`）能导出/对应一个这样的 fd。

### Task 0.1: 摸清 underhill GuestMemory 的 fd 来源

**Files:** 只读探查（无改动）

- [ ] **Step 1: 查 underhill 怎么拿 guest RAM fd**

Run: 探查 `params.guest_memory: &GuestMemory`（`pci_resources/src/lib.rs:34-52`）能否导出 backing fd；underhill 是否持有 `/dev/mshv_vtl_low` 等价物（或 guest memory 的 memfd/mapping fd）。
```bash
cd /home/xp/refs/openvmm
grep -rn "mshv_vtl_low\|VtlLow\|guest_memory_fd\|MemoryBacking\|GuestMemory::new\|fn fd\|as_fd\|RawFd" openhcl/underhill_core/src/ support/mshv* 2>/dev/null | grep -i "fd\|vtl_low\|backing" | head -40
grep -rn "fn .*-> .*Fd\|backing\|mappable\|memory_block\|GuestMemory" vm/devices/pci/pci_resources/src/lib.rs
```
判定：underhill 是否有现成 API 拿到一个"file_offset=GPA 可 mmap"的 fd。若无现成 API，POC 要找：underhill 怎么自己 open `/dev/mshv_vtl_low`（W5a test client 的做法，underhill 同为 VTL2 进程应可）。

- [ ] **Step 2: 最小 POC（standalone 或 ohcldiag-dev 推进真 VTL2）**

写一个最小实验（参照 W5a harness `usnvmemu/experiments/2026-06-12-openhcl-vtl2-deploy/`）：在真 VTL2 内，模拟 underhill 侧——open guest RAM fd（mshv_vtl_low 或 underhill GuestMemory 导出的 fd）→ `VfioUserClient::dma_map(fd)` 给真 firmware → firmware 零拷贝读写一段 guest RAM → 独立 oracle 验证。

判定门：**underhill 能拿到的那种 fd（不是 test client 自开的）经 dma_map 给 firmware 后零拷贝工作**。若 underhill GuestMemory 不导出这种 fd，POC 要确认"underhill 自 open mshv_vtl_low + 按 GPA dma_map"可行（W5a 已强烈暗示可行，POC 坐实）。

- [ ] **Step 3: 记录 POC 结论 + 决定 DMA 集成形态**

写 `usnvmemu/experiments/2026-06-12-w6b-dma-fd-poc/RESULT.md`：underhill 侧 guest RAM fd 来源 + dma_map 形态（整段 map vs 按需）+ 零拷贝验证结果。这决定 Phase 3 的 DMA task。

> **若 POC 失败**（underhill 拿不到可 mmap 的 guest RAM fd）：DMA 退回"firmware 经 control socket 发 ReadGpa/WriteGpa，worker 用 `guest_memory.read_at/write_at`"（模板 pcie_remote 的做法，非零拷贝但确定可行——underhill 的 GuestMemory 一定能 read_at/write_at）。这是 fallback，不是砍 scope：设备仍完整能用，只是 DMA 走消息而非零拷贝。**Phase 3 DMA task 按 POC 结论二选一。**

---

## Phase 1 — 新 crate（基础：cfg 枚举 + MMIO defer-worker + MSI-X eventfd→deliver），standalone 可测

### Task 1.0: 验证 member crate path-dep exclude crate + crate 骨架

**Files:**
- Create: `vm/devices/pci/vfio_user_pci_resources/Cargo.toml` + `src/lib.rs`
- Create: `vm/devices/pci/vfio_user_pci_device/Cargo.toml` + `src/lib.rs`（占位）
- Modify: 根 `Cargo.toml`（workspace members 自动包含 `vm/devices/pci/*`？确认 glob）

- [ ] **Step 1: handle 资源 crate**

`vfio_user_pci_resources/src/lib.rs`（照抄 `pcie_remote_resources` 的 `PcieRemoteVmbusHandle` 形态）：
```rust
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! vfio-user NVMe 设备的 resource handle（W6b）。underhill 把 firmware 的
//! unix socket 路径包成此 handle，resolver 据此 connect + 装配 ChipsetDevice。

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// 一个 vfio-user NVMe firmware 设备（VTL2 内独立进程，AF_UNIX 可达）。
#[derive(MeshPayload)]
pub struct VfioUserNvmeHandle {
    /// guest-visible 实例 ID。
    pub instance_id: guid::Guid,
    /// firmware vfio-user server 的 AF_UNIX socket 路径。
    pub unix_path: String,
}

impl ResourceId<PciDeviceHandleKind> for VfioUserNvmeHandle {
    const ID: &'static str = "vfio_user_nvme";
}
```
Cargo.toml deps：`mesh`、`vm_resource`、`guid`（对齐 pcie_remote_resources/Cargo.toml）。

- [ ] **Step 2: device crate 骨架 + 验 path-dep**

`vfio_user_pci_device/Cargo.toml`：照抄 `pcie_remote_device/Cargo.toml` 的 deps（chipset_device/pci_core/pci_resources/vmcore/vm_resource/guestmem/pal_async/mesh/inspect/async-trait/anyhow/tracing/futures/parking_lot/zerocopy/cvm_tracing/device_emulators）+ **`vfio_user_device = { path = "../../../../usnvmemu/crates/vfio_user_device" }`** + `vfio_user_wire`（path）+ `vfio_user_pci_resources`（path）。

`src/lib.rs` 占位 `pub mod state; pub mod ...;` + 先空。

Run: `cargo build -p vfio_user_pci_device 2>&1 | tail`
Expected: 编译过。**member crate path-dep exclude crate 是 cargo 标准用法（exclude 只影响 `--workspace` 是否自动包含，path-dep 解析不受影响），几乎肯定可行，不需 fallback。**

> **B-2（architect）**：vfio_user_device/wire 确在 exclude（Cargo.toml:73-75）。**绝不**把它们"移进 workspace members"——那会让它们受 workspace lint/dep 统一约束 + 被 `--workspace` 拉入，威胁 W5a 的 standalone musl 可测基建（[[poc-before-settling-design]]：未验证的乐观断言不能写进 plan）。若 path-dep 真不可行（极不可能），**停下来重新设计 crate 边界并 POC 坐实**，不擅自移动 W5a 基建。

### Task 1.1: state + AbsentPcieDevice + lib（照抄模板）

- [ ] **Step 1**: `src/state.rs`（`DeviceState{Connecting,Live,Lost}` + `SharedState`）照抄 `pcie_remote_device/src/state.rs`。
- [ ] **Step 2**: `AbsentPcieDevice`（boot DoS fallback）照抄模板 `lib.rs` 里的定义。
- [ ] **Step 3**: `cargo build -p vfio_user_pci_device` 过。

### Task 1.2: vfio_user_device 全双工 split API（B-1 前提）

**Files:** Modify `usnvmemu/crates/vfio_user_device/src/{lib.rs,client.rs,async_socket.rs}`（W6a crate 扩展）

> **B-1（architect BLOCKING）**：W6b worker 必须**全双工**（发请求 + 收 reply 并发、多个 MMIO read 在途），照抄模板 worker 的 in_flight map 形态。W6a 的 `VfioUserClient` 是串行（Mutex<PolledSocket>，request 内 write 后同函数 read），直接 `.await` region_read 会**阻塞整个 worker select loop**（真死锁风险：firmware 卡住则 worker 连 shutdown 都不响应）。W6a lib.rs 行 23-26 已注记 split 是 W6b 演进路径——现在落地。

- [ ] **Step 1: PolledSocket::split() 全双工通道**

在 `vfio_user_device` 加 `VfioUserChannel`（split 版）：`PolledSocket::split()` 得 read/write 半，分别包成：
- `VfioUserWriter`：`async fn send_command(&mut self, msg_id, cmd, payload, fds) -> Result<()>`（发一帧，不等 reply）。
- `VfioUserReader`：`async fn recv_reply(&mut self) -> Result<WireMessage>`（读一帧 reply，调用方按 msg_id 匹配）。
- `VfioUserClient::into_channel(self) -> (VfioUserWriter, VfioUserReader)`（消费已 handshake 的 client，拆出全双工半）。msg_id 分配移到调用方（worker）。

socket split：`AsyncSocket` 当前持 `Mutex<PolledSocket>`；全双工要 `PolledSocket::split() -> (ReadHalf, WriteHalf)`（pal_async socket.rs:233，要求 `T: AsSockRef + Read + Write`，UnixStream 满足）。读半 cmsg 收（client 不收 fd，纯字节）、写半 cmsg 发 fd。**注意**：split 后两半各自 IO，不共享 Mutex——send/recv 真并发。SCM_RIGHTS 发 fd 在 write 半（照搬 async_socket 的 try_send）。

- [ ] **Step 2: 单测（standalone）**：split 通道 loopback 对接真 `vfio_user_transport` server——并发发 2 个 region_read（不同 msg_id）+ 乱序收 reply 按 msg_id 匹配，验全双工正确（这是 W6a 串行版做不到的，证 split 价值）。

- [ ] **Step 3**: 保留 W6a 的串行 `VfioUserClient`（W5a harness + handshake/identity 仍用它）；split 通道是新增 API，不破坏现有。`cargo test`（vfio_user_device 全绿 + 新 split 测）+ clippy。

### Task 1.3: worker（全双工持 writer+reader + in_flight，MMIO→region_rw，eventfd→deliver）

**Files:** Create `vfio_user_pci_device/src/worker.rs`

- [ ] **Step 1: DeviceRequest + Worker struct（全双工，照抄模板 in_flight）**

```rust
/// device shim → worker 的请求。
pub struct DeviceRequest {
    pub kind: ReqKind,
}
pub enum ReqKind {
    MmioRead { bar: u32, offset: u64, size: usize, token: DeferredRead },
    MmioWrite { bar: u32, offset: u64, data: Vec<u8> },  // fire-and-forget
}

pub struct Worker {
    writer: vfio_user_device::VfioUserWriter,   // 发请求帧
    reader: vfio_user_device::VfioUserReader,   // 收 reply 帧
    next_msg_id: u16,
    in_flight: HashMap<u16, InFlightRead>,      // msg_id → token（照抄模板）
    state: SharedState,
    from_device: mesh::Receiver<DeviceRequest>,
    interrupts: Vec<vmcore::interrupt::Interrupt>,
    _irq_tasks: Vec<pal_async::task::Task<()>>, // eventfd-wait tasks 句柄
    stats: SharedWorkerStats,
}
struct InFlightRead { token: DeferredRead, size: usize, cmd: Command }
```

- [ ] **Step 2: run() select_biased!（全双工双 arm，照抄模板结构）**

```rust
pub async fn run(mut self, mut shutdown: mesh::Receiver<()>) {
    loop {
        let is_lost = matches!(self.state.load(), DeviceState::Lost);
        select_biased! {
            _ = shutdown.next().fuse() => break,
            req = self.from_device.next().fuse() => {
                let Some(req) = req else { break };
                match req.kind {
                    ReqKind::MmioRead { bar, offset, size, token } => {
                        let msg_id = self.alloc_msg_id();
                        self.in_flight.insert(msg_id, InFlightRead { token, size, cmd: Command::RegionRead });
                        // writer 发 RegionRead 帧（立即返回，不等 reply）。
                        if let Err(e) = self.writer.send_region_read(msg_id, bar, offset, size as u32).await {
                            // transport 死 → 失败所有 in_flight + Lost。
                            self.go_lost_drain(e);
                        }
                    }
                    ReqKind::MmioWrite { bar, offset, data } => {
                        let msg_id = self.alloc_msg_id();  // 不插 in_flight（fire-and-forget）
                        if let Err(e) = self.writer.send_region_write(msg_id, bar, offset, &data).await {
                            self.go_lost_drain(e);
                        }
                    }
                }
            }
            // Lost 时不 read（避免 busy-loop，照抄模板 read_inbound_or_pending）。
            inbound = recv_reply_or_pending(&mut self.reader, is_lost).fuse() => {
                match inbound {
                    Ok(reply) => self.dispatch_reply(reply),  // 按 msg_id 匹配 in_flight → token.complete
                    Err(e) => self.go_lost_drain(e),
                }
            }
        }
    }
    self.drain_in_flight();  // complete_error(NoResponse) 所有在途
}
```
`dispatch_reply`：解 reply 的 msg_id → `in_flight.remove(&msg_id)` → 校验 RegionRead echo + 取数据段 → `token.complete(&data[..size])`；未知 msg_id / echo 不符 → 记 stat + （宽容，非致命）。write reply（无 in_flight）丢弃。`drain_in_flight` 照抄模板（complete_error NoResponse）。

> **顺序保证（H-1）**：MMIO write→read 同寄存器的顺序，来自 **worker 单线程 FIFO 消费 from_device + writer 单 socket 顺序发帧**（与 client 串行/全双工无关）。device 的 write 立即 Ok（不 defer）、read defer，但两者的 DeviceRequest 按 guest 访问顺序进 channel，worker FIFO 发帧 → firmware 单连接串行处理 → 顺序保持。

- [ ] **Step 3: eventfd→Interrupt::deliver wait tasks（独立 task，不碰 socket）**

每 MSI-X 向量 i 一个 task：`wait eventfd readable → read 清计数 → interrupts[i].deliver()`。`Interrupt` 是 `Clone+Send`（vmcore/interrupt.rs:31），从独立 task 调 deliver 安全。这些 task 在 resolver spawn（拿 driver+eventfds+interrupts 后），句柄存 worker `_irq_tasks` 防 drop；worker task 自身由 `WorkerTasks` Arc 持有（照抄 resolver.rs:274 生命周期不变量）。**eventfd 是 firmware→underhill 的独立 SCM_RIGHTS fd，不走 control socket**，与 worker 主循环不争用。

- [ ] **Step 4: 单测（standalone，对接真 vfio_user_transport server）**

worker 单测：loopback split 通道对接真 `VfioUserSession`+MockDev → 发 `DeviceRequest::MmioRead` → 验 token complete 出 MockDev 寄存器值；**并发 2 个 MmioRead 验全双工**（不串行阻塞）。

### Task 1.4: device shim（cfg/msix 本地 + MMIO defer→worker）

**Files:** Create `vfio_user_pci_device/src/device.rs`

- [ ] **Step 1**: 照抄 `pcie_remote_device/src/device.rs` 的 `PcieRemoteDevice`，**改名 `VfioUserPciDevice`**，struct 字段同（state/to_worker/cfg_space/msix/side_effect_offsets/next_seq/worker_stats）。`ChipsetDevice`/`PciConfigSpace`/`MmioIntercept` impl **完全照抄**（cfg 本地、MSI-X BAR 本地、其它 BAR MMIO read→defer→`DeviceRequest::MmioRead`、MMIO write→fire-and-forget `DeviceRequest::MmioWrite`）。只把 frame 构造从 protobuf `ToHost{MmioAccess}` 换成 `DeviceRequest{ kind: ReqKind::MmioRead/MmioWrite }`。
- [ ] **Step 2**: 照抄模板的 device 单测（`live_cfg_read_vendor_device` / `mmio_write_returns_ok_immediately_fire_and_forget` / `mmio_write_lost_returns_err` / invalid_size）——这些是纯本地 cfg/mmio 逻辑，standalone 跑。**关键回归：MMIO write 必须立即 Ok 不 Defer**（模板 commit 291d8645 的 nvme.sys hang 教训）。
- [ ] **Step 3**: `cargo test -p vfio_user_pci_device` 过。

### Task 1.5: prepared + spawn（client connect + identity reads）

**Files:** Create `src/prepared.rs` + `src/spawn.rs`

- [ ] **Step 1: prepared**
```rust
pub struct PreparedVfioUserDevice {
    pub client: vfio_user_device::VfioUserClient,
    pub hardware_ids: pci_core::spec::hwid::HardwareIds,
    pub bar0_size: u64,
    pub msix_count: u16,
}
```

- [ ] **Step 2: spawn（client 主动 connect + handshake + identity）**

`spawn_vfio_user_connects(driver, spawner, instances: Vec<(Guid, String/*unix_path*/, Duration)>, prepared) -> Vec<Task>`：每 instance 一个 task，循环（有界重试 + backoff，照抄 handshake_spawn 的 MAX_ACCEPT_ATTEMPTS + timeout 防御）：
```rust
let client = VfioUserClient::connect(&driver, &unix_path).await?;
let mut client = client; // handshake 需 &mut
client.handshake().await?;
let info = client.get_device_info().await?;
let bar0 = client.get_region_info(pci_region::BAR0).await?;
let irq = client.get_irq_info(pci_irq::MSIX).await?;
// identity：读 firmware 的 PCI config header。
let id_hdr = client.region_read(pci_region::CONFIG, 0, 4).await?;  // vendor|device
let class_hdr = client.region_read(pci_region::CONFIG, 0x08, 4).await?; // rev|class
let sub = client.region_read(pci_region::CONFIG, 0x2c, 4).await?;  // subsystem
let hardware_ids = derive_hardware_ids_from_cfg(&id_hdr, &class_hdr, &sub);
prepared.lock().insert(instance_id, PreparedVfioUserDevice {
    client, hardware_ids, bar0_size: bar0.size, msix_count: irq.count as u16,
});
```
`derive_hardware_ids_from_cfg`：**按 PCI CLASS_REVISION 寄存器布局拆**（offset 0x08 的 u32：bit31-24=base_class, 23-16=sub_class, 15-8=prog_if, 7-0=revision；offset 0=vendor|device；0x2c=subsystem），**不是**照抄 protobuf 版 `class_code` 移位（M-2）。NVMe class = base 0x01 / sub 0x08 / prog_if 0x02（spec.rs 确认）。加单测对标 NVMe triple。

- [ ] **Step 3: spawn 单测（loopback 真 server）**：connect 真 `vfio_user_transport` server + MockDev → 验 prepared 的 hardware_ids/bar0_size/msix_count 对（对标 MockDev：vendor 0x1234/device 0x5678/class 0x010802/BAR0 8192/MSIX 8）。

### Task 1.6: resolver（assemble：三件套 + set_irqs + spawn worker + eventfd tasks）

**Files:** Create `src/resolver.rs`

- [ ] **Step 1**: 照抄模板 `resolver.rs` 的 `PreparedMap`/`WorkerTasks` + `AsyncResolveResource<PciDeviceHandleKind, VfioUserNvmeHandle>` + `resolve_one`（缺 prepared → AbsentPcieDevice）。
- [ ] **Step 2: assemble_device（改 async fn，resolve 内 .await）**（照抄模板 + vfio-user 适配）：
  - **assemble_device 改 `async fn`**（H-3）：模板是同步 fn，但 W6b 要在装配期 `.await`（set_irqs/dma_map）。`resolve` 本就 async（resolver.rs:96），`.await assemble_device(...)` 即可；产物 `VfioUserPciDevice` 仍是同步 ChipsetDevice。
  - 三件套：`MsixEmulator::new(MSIX_BAR_INDEX, msix_count, params.msi_target)` + `DeviceBars::new().bar0(bar0_size, Intercept(register_mmio.new_io_region("bar0", bar0_size))).bar4(msix.bar_len(), Intercept(...))` + `ConfigSpaceType0Emulator::new(prep.hardware_ids, vec![Box::new(msix_cap)], Vec::new(), bars)`——照抄。
  - **64-bit BAR0（M-1）**：`DeviceBars::bar0` 经 pci_core **自动呈现为 64-bit BAR**（占 BAR0+BAR1 寄存器对，cfg_space_emu.rs:264-283 对每个 BAR 强制 `with_type_64_bit(true)`）——这正是 NVMe 规范要求，**无需额外处理**。MSI-X 用 BAR4。
  - interrupts vec：`(0..msix_count).map(|i| msix.interrupt(i).unwrap())`。
  - **MSI-X eventfd（H-4 timeout）**：建 `msix_count` 个 eventfd → `prep.client.set_irqs(MSIX, 0, &eventfds).await` **必须用 `CancelContext::with_timeout` 包裹**（resolve 是 boot 关键路径，firmware 不回则 boot 挂——模板 assemble 无 IO 故无此风险，W6b 新增的 await 是新 boot-blocking 点）。超时 → 返回 AbsentPcieDevice（更安全）。eventfd-wait tasks spawn 到 `params.driver_source`，句柄交 worker。
  - **client split**：`prep.client.into_channel()` 拆 writer/reader 交 worker（Task 1.2）。
  - **DMA（Phase 3，按 Phase 0 POC）**：`dma_map(...)` 同样 timeout 包裹。Phase 1 跳过（枚举+MMIO 不需 DMA）。
  - spawn worker（持 writer/reader + interrupts + eventfd tasks 句柄）。
- [ ] **Step 3**: device 单测：模板 resolver 测多 doc-only（params 含 trait object 难构造）。照抄等价 doc 测 + `derive_hardware_ids_from_cfg` 单测覆盖。

### Task 1.7: Phase 1 coarse commit

- [ ] 全 crate `cargo test -p vfio_user_pci_device -p vfio_user_pci_resources` + `cargo clippy -p vfio_user_pci_device -- -D warnings` 过。单 commit（pathspec 限 `vm/devices/pci/vfio_user_pci_*` + 根 Cargo.toml + plan）。

---

## Phase 2 — underhill_core 集成（CLI 注入 + resolver 注册 + connect spawner）

> **⚠️ 详化 gate（执行前必读）**：Phase 2/3 以下任务是**路线图级**（"照抄 pcie_remote 的 X + 真 VM 验证 Y"），**不是 Phase 0/1 那种 no-placeholder 可直接执行的任务**。这是有意的——它们有真实依赖，现在写成 no-placeholder 会是投机：
> - **Phase 2 形态依赖 Phase 1 落地的 crate API**（handle 类型 / resolver / worker 签名）。
> - **Phase 3 的 DMA 形态依赖 Phase 0 POC 结论**（零拷贝 dma_map vs ReadGpa fallback）。
> - underhill_core 接线比"照抄行号"复杂：options.rs 有 instance + takeover(Path C) **两条**注入路径 + env 读取位置 + 端口黑名单 + K-19 timeout cap；vtl2_settings_worker 的 VPCI offer 机制需 vmwp 已知 GUID。这些需在执行时**实读代码**。
>
> **因此：Phase 0 + Phase 1 落地后，对 Phase 2/3 做一次专门的详化 pass**——explore underhill_core 的 options.rs(315-432 区域 + env 读取点)/vtl2_settings_worker.rs(VPCI push)/worker.rs(2308-2391 注册块) 到代码形态深度 + 据 Phase 0 POC 定 Phase 3 DMA 形态 → 写成 no-placeholder 任务 → architect review → 再执行。下面是这次详化的**骨架与目标**，不是最终可执行任务。

> 改 underhill_core；**需 IGVM 重建 + 真 VM 才能验证**。先 realize-only（设备注册但 boot 不挂），再加 cfg（guest 枚举）。

### Task 2.1: CLI env 注入

**Files:** Modify `openhcl/underhill_core/src/options.rs`

- [ ] 照抄 `PcieRemoteCliConfig`（options.rs:333-432）做 `VfioUserNvmeCliConfig{ instance_id: Guid, unix_path: String }`，env `OPENHCL_VFIO_USER_NVME=<guid>:<unix_path>[;...]`，`FromStr` 解析。

### Task 2.2: vtl2_settings_worker push VPCI device

**Files:** Modify `openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs`

- [ ] 照抄 pcie_remote 的 `vpci_devices.push(UhVpciDeviceConfig{ instance_id, resource: VfioUserNvmeHandle{instance_id, unix_path}.into_resource() })`（vtl2_settings_worker.rs:1909-1923 类比）。

### Task 2.3: worker.rs resolver 注册 + connect spawner

**Files:** Modify `openhcl/underhill_core/src/worker.rs` + `Cargo.toml`

- [ ] 照抄 pcie_remote 的注册块（worker.rs:2308-2391）：建 `PreparedMap`/`WorkerTasks` Arc + CVM gate（CVM 关掉本特性）+ `spawn_vfio_user_connects(...)` 起 connect tasks + boot grace poll + `resolver.add_async_resolver::<PciDeviceHandleKind, _, VfioUserNvmeHandle, _>(VfioUserPciResolver::new(...))` + detach。env-gated（无 `OPENHCL_VFIO_USER_NVME` 则整块跳过）。

### Task 2.4: IGVM 构建 + realize-only 真 VM 验证

- [ ] **Step 1**: `cargo xflowey build-igvm x64 --release`（WSL）→ 产物 `flowey-out/artifacts/build-igvm/release/x64/openhcl-x64.bin`。
- [ ] **Step 2**: 部署到真 VM（复用 hyperv_interop harness 改 IGVM 来源 + firmware 启 vfio-user server，env 注入 socket 路径）。**先验 boot 不挂**（设备注册成功，ohcldiag-dev inspect 确认 assembled + handshake ok）。
- [ ] **Step 3**: 加 cfg（已在 Phase 1）→ 真 VM 验 **guest `lspci` 看到 NVMe 设备**（vendor/device/class 01:08:02 + BAR0 size + MSI-X cap）。这是第一个真机里程碑。
- [ ] **Step 4**: Phase 2 coarse commit（underhill_core 改动 + harness）。

---

## Phase 3 — DMA 零拷贝 + 真 IO（设备真能用）

> 按 Phase 0 POC 结论接 DMA；这是"完整能用"的核心。

### Task 3.1: realize 时 dma_map guest RAM 给 firmware

- [ ] 按 Phase 0 POC：resolve 时 underhill 拿 guest RAM fd → `prep.client.dma_map(gpa_base, size, RW, fd, fd_offset).await`（timeout 包裹）整段映射（或按 POC 决定的形态）。**若 POC 走 fallback**（ReadGpa 消息）：worker 加回模板的 `handle_read_gpa/write_gpa`（用 `params.guest_memory.read_at/write_at`）**+ 一并加回模板的 `DmaRate` 速率限制**（M-3：fallback 走 control socket DMA 字节流，rate limit 重新相关；零拷贝路径才不需要）；device 不需改。

### Task 3.2: MMIO BAR0 寄存器路径真通（driver probe）

- [ ] 真 VM：guest nvme driver attach → 读 BAR0 CAP/CSTS（经 device MMIO read defer→worker→`region_read`）→ 写 CC.EN/AQA/ASQ/ACQ（fire-and-forget `region_write`）→ poll CSTS.RDY。验 driver 成功 enable controller。

### Task 3.3: 真 IO + MSI-X 中断送达

- [ ] 真 VM e2e：guest 建 IO queue + 提交 NVMe IO（read/write）→ firmware 经 dma_map 零拷贝访问 guest queue/data → 完成 → fire eventfd → worker `interrupts[i].deliver()` → guest 收 MSI-X 中断 → IO 完成。**独立 oracle**（host 侧读 backing file 验数据）。这是 W6b 完整判据：**guest 真能用这个 vfio-user NVMe 设备做 IO**。
- [ ] Phase 3 coarse commit（DMA + 真 VM 验证修复）。

---

## Self-Review

**1. Scope coverage（完整有意义）**：W6b = guest 枚举（Phase 1 cfg + Phase 2 集成）+ driver 驱动（Phase 3.2 MMIO）+ 真 IO（Phase 3.3 DMA + 中断）。不停在"lspci 看到"。✓ 符合 [[meaningful-complete-not-minimal]]。
**2. 承重 POC 前置**：Phase 0 验 underhill→firmware DMA fd-passing；失败有 fallback（ReadGpa 消息，设备仍完整能用）。✓ [[poc-before-settling-design]]。
**3. standalone-first**：Phase 1 全 crate 逻辑 loopback 单测；IGVM/真 VM 只验集成接缝 + guest 真行为。✓
**4. boot 风险**：env-gated + CVM-gated + AbsentPcieDevice + 有界 timeout + realize-only 先行。✓
**5. 模板忠实**：device/worker/resolver/spawn 照抄 pcie_remote 结构 + 防御 pattern，只换 wire 层（protobuf→VfioUserClient）。fire-and-forget MMIO write 回归测保留。✓
**6. Placeholder**：Phase 0/2/3 的真 VM step 是真实外部验证（非占位）；新 crate 逻辑给了 struct/方法骨架 + 照抄指向。执行时 device/worker/resolver 大量照抄模板，plan 给适配点。**注意**：本 plan 对照抄部分用"照抄模板 + 改 X"而非全文复制（模板已 proven，复制千行无益）——执行者须实际读模板文件落地。
**7. 待定**：member crate path-dep exclude crate 的可行性（Task 1.0 验，有 fallback）；underhill GuestMemory fd 来源（Phase 0 POC 定）。

## 风险
- **IGVM 慢循环（5-20 min/迭代）**：Phase 1 全 standalone 单测吸收逻辑 bug；IGVM 只验集成。
- **underhill boot 风险**：env+CVM gate + realize-only 先行 + AbsentPcieDevice。
- **承重 DMA fd-passing 未验**：Phase 0 POC 前置 + fallback。
- **多 phase 多会话**：Phase 0/1 可本会话 standalone 推进；Phase 2/3 需 IGVM+真 VM，可能跨会话。这不是"为小而小"拆分，是 standalone vs 真 VM 的自然边界。
- **MMIO write fire-and-forget + region_write async**：device 立即返 Ok，worker 异步发 region_write——若 worker 落后，写顺序 vs 后续读可能 race。模板靠 host 串行处理；W6b 的 firmware 也串行处理 region wire（单连接），且 MMIO 通常 driver 串行。Phase 3.2 真 VM 验证关注此。
