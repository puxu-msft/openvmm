# OpenHCL / OpenVMM 远程 PCIe 实验设备：设计文档 (v3.1)

- **日期**：2026-05-29
- **作者**：xp（与 Claude Code 共同设计）
- **状态**：草案 v3.1（**3 轮独立 reviewer 评审完成；P0 全部收口；P1/P2 列入 §10 跟进**，可进入实施计划）

> **v3 → v3.1 修订**（吸收 Round 3 reviewer 反馈）
>
> 1. **Resolver 改为动态注册**（v3 错误地塞进 `register_static_resolvers!`，但该宏要求 unit struct + const 构造，不能持 `Arc<Mutex<HashMap>>`）。正解参照 `VfioDeviceResolver`：在 worker 启动期 `resolver.add_async_resolver(...)`，resolver 自持 `prepared_map: Arc<Mutex<HashMap>>` + `_tasks: Vec<Task<()>>`。
> 2. **CVM 兜底真造 `AbsentPcieDevice` stub**（v3 写"返回 sentinel"但 `AsyncResolveResource::resolve` 返回 `Result<ResolvedPciDevice, _>` 没有 None 变体；"静默忽略"在 Rust 不存在）。
> 3. **接线点行号收口到单点描述**（v3 三处行号互不一致）。
> 4. **transport trait 明写 `futures::io::AsyncRead + AsyncWrite + Send + Unpin + 'static`** + 用泛型而非 trait object（与 `pal_async` 仓库范式一致）。
> 5. **vsock 端口黑名单数值修正**（v3 写错 `0x7053/0x7054`，实际是 `1/2`）。
> 6. **task 归属统一**：进 Resolver `_tasks: Vec<Task<()>>`（v3 三处描述不一致）。
- **支持的部署形态**：
  - **A. OpenVMM 启动 OpenHCL（开发/petri）**：CLI 直接传，最容易。
  - **B. OpenHCL + 自建 IGVM**：用户 fork 后 rebuild IGVM 用 `APPEND_CHOSEN` cmdline 注入。
  - **C. OpenHCL + 生产 Hyper-V + "占位 NVMe controller" 接管**：用户在 Hyper-V 配置一个空 NVMe，OpenHCL 拦截该 instance_id 路由到 pcie_remote，vmwp 已知 GUID → vpci OFFER 通过。
  - **D. OpenVMM-only 实验**（独立形态）：现有 `--pcie-remote` CLI 直接走 OpenVMM 用户态 VMM 路径，仓库已有一半骨架，补齐 resolver + device crate 即可。

---

## 1. 目标

让 Windows guest 看到一个 PCIe 设备，所有设备语义实现在 Windows host 用户态实验程序里。OpenHCL/OpenVMM 内只保留极薄外壳。

**显式目标**

- 不追求性能（实验/调试用途）。
- OpenHCL/OpenVMM 内的设备代码尽可能薄。
- 设备类型对外是"通用 PCIe 设备"。host 用户态可以把它"伪装"成 NVMe controller，也可以装别的；薄壳不感知。
- host 用户态程序不在 / 崩溃 / 重启时，对 guest 表现为 **PCIe 合规的"设备丢失"**（cfg/MMIO 读全 1），驱动可走 surprise-down 路径。
- **OpenHCL 在生产 Hyper-V 上可用**（通过 §3.1 的占位 NVMe 路径），不依赖 Microsoft 改 vmwp/VSP。
- **OpenVMM 路径不放弃**：仓库现有 `--pcie-remote` CLI、`PcieRemoteHandle`、`GenericPcieRootComplex` 都已经存在；补齐 resolver + 实际 device 实现，OpenVMM-only 形态独立可用。**OpenHCL 与 OpenVMM 共享 protocol crate 与 device crate**（仅 handle / 通道适配层不同）。

**显式非目标（YAGNI）**

- 不实现共享内存 DMA、不实现 NVMe Fast-Path、不实现 SR-IOV / ATS / PASID。
- **不支持机密 VM**（SNP/TDX/VBS）—— 见 §3.10。
- 不在 OpenHCL 内实例化 emulated PCIe Root Complex（用 vpci）。
- 不支持热插拔由 host 任意触发；hotplug 在 v1 不支持。
- **不支持 save/restore / servicing**（v1 用 `SavedStateNotSupported`）。
- 不提供 IOMMU 等价的设备级 DMA 隔离。

---

## 2. 范围、信任边界、与 host 的协作

| 在范围内 | 不在范围内 |
|---------|-----------|
| 4 种部署形态（§1 A/B/C/D） | host 端 vmwp/VSP 任何改动 |
| **共享**：pcie_remote_protocol + pcie_remote_device + host SDK 三个 crate 设计 | host 端实验程序的实现 |
| OpenHCL 适配：vpci 接入 + 占位 NVMe 接管路径 + CLI 注入路径 | OpenHCL 内自造 emulated PCIe Root Complex |
| OpenVMM 适配：复用现有 GenericPcieRootComplex | 已有 `PcieRemoteHandle.socket_addr` 字段的废弃流程（保留兼容） |
| vsock 数据面 (`AF_VSOCK` ↔ `AF_HYPERV`)；OpenVMM 形态可继续走 TCP loopback | TCP / hvsocket relay / 新 vmbus device class |
| 仅非 CVM 启用 | CVM 启用路径 |

**信任与协作边界（v2 模糊，v3 收口）**：

| 层 | 改动需求 | 谁 own |
|----|---------|--------|
| **vsock 数据面** | 注册 service GUID + ACL（一次性 setup.ps1） | 用户运维 |
| **vpci 控制面（OFFER/REVOKE）** | vmwp 必须已知 bus_instance_id | 必须借用已有 NVMe GUID（C 形态）或自建 IGVM（B 形态）或 OpenVMM 启动（A 形态） |
| **OpenHCL cmdline 注入** | host 必须能 append cmdline（IGVM 的 `APPEND_CHOSEN` 策略）| 形态 B 自建 IGVM；形态 A OpenVMM 直传 |
| **OpenVMM-only 形态** | 不涉及 vpci/vmwp | 用户启动 OpenVMM 进程 |

---

## 3. 关键架构决策

### 3.1 OpenHCL 接入：vpci channel；三种 instance_id 获取路径

**决策**：OpenHCL 中设备表现为 **vpci 设备**。每个实例独立 `HclVpciBusControl`（[openhcl/underhill_core/src/vpci.rs](../../../openhcl/underhill_core/src/vpci.rs)），slot 固定 vpci 默认（0）。

**获取 vmwp 已知 instance_id 的三种路径**：

- **C. 占位 NVMe 接管（生产 Hyper-V 默认）**
  - 用户用 PowerShell `Add-VMNvmeController` + `Set-VMNvmeController` 注册一个 NVMe controller，**不绑定后端磁盘**。
  - 用户用 `Set-VMNvmeController -Id <guid> -Tag "pcie_remote"` 或在 OpenHCL CLI 用 `--pcie-remote-take-over <guid>:<vsock_port>` 声明 "这个 GUID 不要按 NVMe 处理，路由到 pcie_remote"。
  - OpenHCL 在 `vtl2_settings_worker.rs:1453` 附近的 `make_nvme_controller_config` **之前**插一个过滤器：列出来的 NVMe controller GUID 若在 takeover 白名单中，**改成构造 `UhVpciDeviceConfig { resource: PcieRemoteVmbusHandle.into_resource() }`** 而非 `NvmeControllerHandle`。
  - vmwp 看到的还是个 NVMe controller GUID（vpci OFFER 通过）；OpenHCL 端实际暴露 pcie_remote 设备给 guest。
  - 风险：guest 看到的设备 vendor/device ID 不是 NVMe class code → guest NVMe 驱动不会绑定（正是我们想要）；vpci 总线本身不关心后端是什么 device class。
- **B. 自建 IGVM cmdline**（用户已声明 fork & private，可接受）
  - IGVM 构建时 cmdline 策略 = `APPEND_CHOSEN`，host 启动时 append `--pcie-remote-instance <guid>,<vsock_port>`。
  - **但 instance_id 仍需 vmwp 已知** —— 所以这条路实质等价于 C，仅 cmdline 注入方式不同。
  - 实操中 B 通常配合 C：用 cmdline append 替代 takeover 文件。
- **A. OpenVMM 启动 OpenHCL（开发/petri）**
  - OpenVMM 自己模拟 vmwp 角色，instance_id 由 OpenVMM CLI 给（已实现 `--pcie-remote`）。
  - 这条路径下，OpenVMM **既**作为 host root partition 提供 vmbus/vpci，**又**作为 host 的"用户态实验程序"的容器（或将实验程序作为独立进程 connect vsock）。

**约束（vpci 路径强加）**：

- vpci 在 cfg-write RMW 时使用 [vm/devices/pci/vpci/src/bus.rs:390](../../../vm/devices/pci/vpci/src/bus.rs#L390) `pci_cfg_read.now_or_never()`，**cfg_read 返回 `Defer` 会被静默替换为 0**（不是 all-1）。
- 因此规定 **cfg_read 100% 同步返回**：Live 状态本地命中；Lost 返回 `IoResult::Err(InvalidRegister)`，让 vpci 走 `data.fill(!0)` 路径（[bus.rs:300](../../../vm/devices/pci/vpci/src/bus.rs#L300)）。
- 建议 instance 数 **≤ 8** soft limit（vpci OFFER 在 GED 上是 ordered host request，多 instance 会串行 + guest BIOS PCI 子系统对单 VM 的 vpci bus 数量在实践中未充分验证）。

### 3.1bis OpenVMM 接入：复用 GenericPcieRootComplex

**决策**：OpenVMM 路径下，**接入用现有 emulated PCIe Root Complex**（[openvmm/openvmm_core/src/worker/dispatch.rs](../../../openvmm/openvmm_core/src/worker/dispatch.rs) L1818–L2812）。

- 现有 [openvmm/openvmm_entry/src/lib.rs:610](../../../openvmm/openvmm_entry/src/lib.rs#L610) 已构造 `PcieDeviceConfig { resource: PcieRemoteHandle.into_resource() }` 塞进 `pcie_devices`，[openvmm/openvmm_core/src/worker/dispatch.rs:236](../../../openvmm/openvmm_core/src/worker/dispatch.rs#L236) 已消费。
- 缺：`ResolveResource<PciDeviceHandleKind, PcieRemoteHandle>` 实现 + 实际 device 实现 + 通道层。
- 通道：OpenVMM 路径**可选 TCP loopback 或 vsock-on-Linux/Windows**。考虑到 OpenVMM 跨平台、用户可能在 WSL 内启动并连本地 host 进程，**v1 保留 TCP loopback**（兼容现有 CLI `socket=localhost:48914`），同时**允许显式选 vsock**（与 OpenHCL 共用 transport 后端）。
- handshake / DeviceDescribe / protocol 完全复用，**仅 transport 适配层不同**。

### 3.2 传输：vsock（OpenHCL 主）+ TCP loopback（OpenVMM 可选）

**OpenHCL 端**：

- 用 [support/vmsocket/](../../../support/vmsocket/) 的 `VmListener::bind(VmAddress::vsock_any(port))`，每个实例独立 vsock 端口。
- Windows host 用 `AF_HYPERV` socket 连 `(target_vm_id, service_id_derived_from_port)`，service GUID 由 `HV_GUID_VSOCK_TEMPLATE` 与端口号合成（[support/vmsocket/src/af_hyperv.rs:31](../../../support/vmsocket/src/af_hyperv.rs#L31)）。
- **service GUID 注册表 ACL 是用户运维步骤**（spec 提供 `setup.ps1` 模板，SDDL `D:P(A;;GA;;;BA)(A;;GA;;;SY)` 仅 Admin/SYSTEM）。OpenHCL 启动期若 bind 失败 → warn + 跳过该实例，**不 boot fail**。

**OpenVMM 端**：

- 默认 TCP loopback（兼容现有 CLI `socket=` 参数）。OpenVMM 是用户态 VMM，自己启动 host 实验程序进程；socket 仅 bind `127.0.0.1`。
- **WSL2 警告**（v3.1 新增）：WSL2 默认开启 `localhostForwarding`，OpenVMM 在 WSL 内 bind `127.0.0.1` 会被自动转发到 Windows host —— **任何 Windows 用户进程都能 connect 该端口**。生产 / 多用户 Windows host 必须改用 vsock 后端或 Unix domain socket。
- 可选 vsock：与 OpenHCL 路径共用 transport trait。

**transport trait**（v3.1 明确）：

```rust
pub trait Transport: futures::io::AsyncRead + futures::io::AsyncWrite + Send + Unpin + 'static {}
```

- 用**泛型**而非 trait object（与仓库 `pal_async` 范式一致，[support/pal/pal_async/src/socket.rs:297](../../../support/pal/pal_async/src/socket.rs#L297) `PolledSocket` 直接 impl `futures::io::AsyncRead/Write`）。
- `VsockTransport = PolledSocket<VmStream>`；`TcpTransport = PolledSocket<socket2::Socket>`（从 `TcpStream::into()`）。
- vmsocket 跨平台（[support/vmsocket/src/lib.rs:9](../../../support/vmsocket/src/lib.rs#L9) `#![cfg(any(windows, target_os = "linux"))]`），**两种部署形态都可用**。

**端口冲突保护**（v3.1 数值修正）：解析 CLI `vsock_port` 时拒绝以下值：

| 端口（十进制） | 来源 |
|--------------|------|
| `1` | `VSOCK_CONTROL_PORT` ([openhcl/diag_proto/src/lib.rs:21](../../../openhcl/diag_proto/src/lib.rs#L21)) |
| `2` | `VSOCK_DATA_PORT` ([openhcl/diag_proto/src/lib.rs:22](../../../openhcl/diag_proto/src/lib.rs#L22)) |
| `3` | VNC 默认 ([openhcl/underhill_core/src/options.rs:543](../../../openhcl/underhill_core/src/options.rs#L543)) |
| `4` | gdbstub 默认 ([openhcl/underhill_core/src/options.rs:546](../../../openhcl/underhill_core/src/options.rs#L546)) |
| `0x1337` | `PIPETTE_VSOCK_PORT` ([petri/pipette_protocol/src/lib.rs:17](../../../petri/pipette_protocol/src/lib.rs#L17)) |
| 任意与 `opt.vnc_port`/`opt.gdbstub_port` 重合 | 运行时校验 |

建议 CLI 解析期推荐 `vsock_port ≥ 0x10000`。

### 3.3 控制反转：通道建立 + 握手在 worker 启动期完成；resolver 动态注册并自持 prepared_map

**handle 模型**（v3 修正 v2 致命错；v3.1 微调 derive 与 Guid 类型来源）：

`PcieRemoteVmbusHandle`（OpenHCL）/ `PcieRemoteTcpHandle`（OpenVMM）的 MeshPayload 字段**只包含可序列化标识符**：
```rust
use guid::Guid;

#[derive(Debug, Clone, MeshPayload)]
pub struct PcieRemoteVmbusHandle {
    pub instance_id: Guid,
    pub vsock_port: u32,
    pub handshake_timeout_ms: u32,
}
```
**不内嵌** OneshotReceiver / Task / mesh::Sender —— v2 设计编不过（[support/mesh/mesh_channel_core/src/oneshot.rs:314](../../../support/mesh/mesh_channel_core/src/oneshot.rs#L314) OneshotReceiver 独占消耗 + Resource::dyn_resolve 可能 round-trip + `Task` 完全不是 MeshPayload）。

**Resolver 注册路径（v3.1 关键修正）**：

v3 错误地选了 `register_static_resolvers!`。该宏（[vm/vmcore/vm_resource/src/lib.rs:371](../../../vm/vmcore/vm_resource/src/lib.rs#L371)）展开为 `#[linkme::distributed_slice]` 的 const slice，引用静态 resolver 实例 `&$resolver`，**要求 resolver 是 ZST 或 const-constructible** —— 不能持 `Arc<Mutex<HashMap>>`（const 上下文无 `Arc::new`）。仓库 unit-struct resolver 范式见 [vm/devices/storage/nvme/src/resolver.rs:29](../../../vm/devices/storage/nvme/src/resolver.rs#L29) `pub struct NvmeControllerResolver;`。

**正解**（与 `VfioDeviceResolver` 完全对齐，[vm/devices/pci/vfio_assigned_device/src/resolver.rs:26](../../../vm/devices/pci/vfio_assigned_device/src/resolver.rs#L26)）：

```rust
pub struct PcieRemoteVmbusResolver {
    prepared: Arc<Mutex<HashMap<Guid, PreparedPcieRemoteDevice>>>,
    _tasks: Vec<Task<()>>,                  // task 归属在此（统一）
}

impl PcieRemoteVmbusResolver {
    pub fn new(prepared: Arc<Mutex<HashMap<Guid, PreparedPcieRemoteDevice>>>,
               tasks: Vec<Task<()>>) -> Self { ... }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, PcieRemoteVmbusHandle>
    for PcieRemoteVmbusResolver { ... }
```

`underhill_core::worker` 启动期：

1. CVM 检查 + 静默过滤（§3.10）。
2. 对每个 instance 调 `handshake::run_listener_and_handshake(...)`，用 `futures::stream::FuturesUnordered` + per-instance timeout，**单个 instance 失败不阻塞其他**（v3 写 `join_all` 不准确）。
3. 成功 instance 结果 = `(prepared: PreparedPcieRemoteDevice, task: Task<()>)`。
4. 汇总 `prepared_map: Arc<Mutex<HashMap<Guid, _>>>` + `tasks: Vec<Task<()>>`，传给 `PcieRemoteVmbusResolver::new(...)`。
5. **动态注册**：`resolver.add_async_resolver::<PciDeviceHandleKind, _, PcieRemoteVmbusHandle, _>(my_resolver)`（参考 [openhcl/underhill_core/src/worker.rs:2287](../../../openhcl/underhill_core/src/worker.rs#L2287)）。
6. 仅把 `PcieRemoteVmbusHandle { instance_id, ... }` 加入 vpci 设备列表。

Resolver 的 `AsyncResolveResource::resolve()`：

```rust
async fn resolve(&self, _: &ResourceResolver, handle: PcieRemoteVmbusHandle,
                 params: ResolvePciDeviceHandleParams<'_>) -> Result<ResolvedPciDevice, Error> {
    let prepared = self.prepared.lock().remove(&handle.instance_id)
        .ok_or(Error::HandshakeNotPrepared(handle.instance_id))?;
    // 同步组装：cfg_emu + bars + msix + PcieRemoteDevice
    let device = build_device_sync(prepared, params)?;
    Ok(ResolvedPciDevice(Box::new(device)))
}
```

**Resolver 不做真正 I/O，仅一次 map.lock().remove()**；async 标签是 trait 要求，不实际 await。

**含义**：

- handshake 失败 / 超时 / CVM 过滤 → **不**加入 vpci 设备列表 → guest 看到 vpci bus 但没有该设备 → **绝不 boot fail**。
- 整体 boot 阻塞 ≤ `max(各 handshake_timeout_ms)`（默认 2s）；`handshake_timeout_ms` 必须 ≤ underhill_core 的 `config_timeout` 一半，CLI 解析期校验。
- §3.8 不再说"完全不阻塞 boot"，改说"最多阻塞 max(handshake_timeout)"。

### 3.4 协议：prost + length-prefixed + 严格上限 + **little-endian**

vsock 是流式 socket，需应用层 framing；TCP 同样需要。手写 codec 基于 `futures::io::AsyncReadExt::read_exact`，无 unsafe。

```protobuf
// pcie_remote_protocol/proto/pcie_remote.proto
syntax = "proto3";
package openhcl.pcie_remote.v1;

message Hello {  // OpenHCL/OpenVMM → host
    uint32 magic = 1;           // 固定 0x52504345 ('RPCE')
    uint32 version = 2;         // 当前 1
    bytes instance_id = 3;      // 16 字节 GUID（运维去重，非认证）
}

message HelloAck {  // host → OpenHCL/OpenVMM
    bool ok = 1;
    string reason = 2;
    DeviceDescribe device = 3;
}

message DeviceDescribe {
    uint32 vendor_id = 1;
    uint32 device_id = 2;
    uint32 class_code = 3;
    uint32 revision = 4;
    uint32 subsystem_vendor = 5;
    uint32 subsystem_device = 6;
    repeated BarInfo bars = 7;
    uint32 msix_count = 8;
    repeated CapabilityBlob capabilities = 9;
    repeated uint32 cfg_write_side_effect_offsets = 10;
}

message BarInfo {
    uint32 index = 1;
    uint64 size = 2;     // 2 幂、≥ 4096
    enum Kind { MMIO_32 = 0; MMIO_64 = 1; }   // 不允许 IO BAR
    Kind kind = 3;
    bool prefetchable = 4;
}

message CapabilityBlob {
    uint32 cap_id = 1;
    bytes raw = 2;       // 长度必须 4 对齐
}

message ToHost {
    uint64 seq = 1;
    oneof body {
        MmioAccess mmio_write = 11;
        MmioAccess mmio_read = 12;
        CfgAccess cfg_write_side_effect = 13;
        Reset reset = 16;
    }
}

message ToOpenhcl {
    uint64 seq = 1;
    oneof body {
        MmioReadResult mmio_read_result = 11;
        ReadGpaRequest read_gpa = 12;
        WriteGpaRequest write_gpa = 13;
        InterruptFire interrupt_fire = 14;
        // 预留 oneof dma_transport
    }
}

// 所有多字节数值在协议中统一 little-endian（PCIe 原生即 LE）。
// 接收方将 uint64 value 按 size 截断后用 little-endian 解释。
message MmioAccess { uint32 bar = 1; uint64 offset = 2; uint32 size = 3; uint64 value = 4; }
message MmioReadResult { uint64 value = 1; }
message CfgAccess { uint32 offset = 1; uint32 size = 2; uint32 value = 3; }
message InterruptFire { uint32 msix_index = 1; }
message ReadGpaRequest  { uint64 token = 1; uint64 gpa = 2; uint32 len = 3; }  // len ≤ 64 KiB
message WriteGpaRequest { uint64 token = 1; uint64 gpa = 2; bytes data = 3; } // data ≤ 64 KiB
message DmaCompletion   { uint64 token = 1; bool ok = 2; bytes data = 3; } // data ≤ 64 KiB (MAX_DMA_BYTES)
message Reset           { uint32 kind = 1; }
```

**协议约束**

- 帧 = 4 字节 LE 长度 + protobuf 负载（vsock/TCP 流式必需）。
- 单帧 ≤ **1 MiB**。
- `WriteGpaRequest.data` / `DmaCompletion.data` ≤ **64 KiB**。
- prost `decode` 显式 `recursion_limit ≤ 8`。
- 每帧带 `seq`；in-flight 表用 seq 配对。
- 每帧严格 1 个 oneof body。
- 字节序统一 **little-endian**（host SDK 用 `value.to_le_bytes()[..size]`；OpenHCL 端 `DeferredRead::complete(&value.to_le_bytes()[..size])`）。
- 任何 schema 错 / seq 不存在 / 状态机违规 → 关 socket → 进 Lost。

### 3.5 同步模型

**cfg space**：

- 读：**100% 本地命中**。Live 走 `ConfigSpaceType0Emulator`；Lost 返回 `IoResult::Err(InvalidRegister)`。
- 写：标准寄存器（BAR/Command/Status）本地；**仅** `cfg_write_side_effect_offsets` 列表内 offset 走 `cfg_write_side_effect` fire-and-forget。
- capability 区由 `ConfigSpaceType0Emulator::write_u32` 内部路由（[cfg_space_emu.rs:613](../../../vm/devices/pci/pci_core/src/cfg_space_emu.rs#L613)），不会到外层。

**MMIO**：

- 读/写走 `IoResult::Defer`。访问尺寸**硬限制 ∈ {1,2,4,8}**（[deferred.rs:43](../../../vm/chipset_device/src/io/deferred.rs#L43) 限定 `(u64, usize)`）。
- 单次 50ms 超时 → `complete_error(IoError::NoResponse)`。
- **dead-man switch**（v3 与运维体感对齐）：
  - **并联两条**：
    1. 连续 **≥4 次** 超时 → 立即 Lost（不让恶意 host 用比例阈值绕过）。
    2. 滑窗 1s 内 **≥8 次** 超时**且**占比 **≥50%** → Lost。

### 3.6 BAR 与 MSI-X：本地仿真，参照 vfio_assigned_device

- BAR：HelloAck 拿到 `Vec<BarInfo>` → 构造 `DeviceBars`。所有 BAR 必须 MMIO_32/64（不允许 IO BAR），size 2 幂且 ≥ 4096。BAR 重映射本地 cache，参考 [vm/devices/pci/vfio_assigned_device/src/lib.rs:559](../../../vm/devices/pci/vfio_assigned_device/src/lib.rs#L559)。
- MSI-X：`MsixEmulator::new(table_bir, msix_count, msi_target.clone())` 在 resolver 同步组装时构造（`MsiTarget: Clone`，[msi.rs:124](../../../vm/devices/pci/pci_core/src/msi.rs#L124)）。`Vec<Interrupt>` 通过 mesh sender 移交后台 task。host 发 `InterruptFire { msix_index }`，后台 task 调 `interrupts[msix_index].deliver()`。**host 拿不到 raw vector**。
- **HelloAck 校验**（必须在 handshake 期完成；与 `cfg_space_emu.rs:230-244` 内部 assert 一一对应）：
  - 每个 cap.raw 长度 4 对齐
  - 累加扩展 cap 长度 ≤ `EXT_CAP_END - EXT_CAP_START`
  - `CapabilityBlob.raw` 长度匹配
  - 任意一条失败 → handshake fail → 不创建设备
- **测试矩阵必须针对 emulator 每个 assert 一对一注入测试**。

### 3.7 DMA：GPA RPC，**不提供** IOMMU 等价隔离

**决策**：

- host 通过 `ReadGpa / WriteGpa` 让 OpenHCL/OpenVMM 代为读写 guest 物理地址。
- OpenHCL 端 guest memory handle **强制为 `gm.vtl0()`**。OpenVMM 端走默认 guest memory。
- 范围校验：`gpa + len ≤ ram_end`，排除 MMIO hole；单次 ≤ 64 KiB；累计速率限制（默认 64 MiB/s，CLI 可调）。
- 协议 `oneof dma_transport` 预留，v1 不实现。

**安全声明**：

> 本设计 **不提供 IOMMU 等价的设备级 DMA 隔离**。host 端实验程序在协议允许范围内可读写 VTL0 RAM 上任意 GPA（除 MMIO 孔与 VTL2 私有页）。VTL2/VTL0 隔离与机密内存保护由 OpenHCL 既有机制 + CVM 硬禁提供。
>
> **host 实验程序的可信级别必须 ≥ Hyper-V VSP**。Service GUID 注册表 ACL（仅 Admin/SYSTEM）+ 用户负责不在 host 端运行不可信代码 = 实际防线。

### 3.8 断连/host 缺席：单一行为 = "设备丢失"

**状态机**：

```
       ┌─────────────────────────────────────────┐
       │ Connecting (boot 时；最多阻塞 handshake_timeout)│
       │  - bind vsock (失败 → warn + skip)            │
       │  - accept (有超时)                           │
       │  - 发 Hello                                   │
       │  - 等 HelloAck (有超时)                      │
       │  - 校验 DeviceDescribe (包括 cap raw)         │
       │  - 失败时 listener 保持开放重新 accept (背靠超时) │
       └─────────────────────────────────────────┘
              │ accept ok + HelloAck ok + 校验 ok
              ↓
       ┌─────────────────────────────────────────┐
       │ Live                                      │
       │  - cfg read 本地                           │
       │  - cfg write 本地 (+ side effect fire-forget)│
       │  - MMIO defer ↔ host RPC                  │
       │  - dead-man switch 监控                    │
       └─────────────────────────────────────────┘
              │ socket EOF / protocol error /
              │ 帧超限 / dead-man trip
              ↓
       ┌─────────────────────────────────────────┐
       │ Lost (terminal in v1)                     │
       │  - cfg read: Err(InvalidReg) → vpci fill !0│
       │  - cfg write: ignore                       │
       │  - MMIO: complete_error(NoResponse)        │
       │  - 同步 drain in-flight 表 (complete_error) │
       │  - drop listener + drop stream             │
       │  - worker task 自然退出                    │
       │  - v1 不支持重连 (端口占用直到进程退出)    │
       └─────────────────────────────────────────┘
```

**Connecting 阶段的健壮性**：

- bind 失败：warn + 跳过。
- handshake 期任何错误 → **listener 不关**，背靠总超时继续 accept 下一个连接，避免恶意第一连"先到永久 wedge"。带 100ms backoff 防 busy spin。
- 总超时到 → 该 instance 不进入 prepared_map → 不出现在 vpci 列表。

**OpenVMM 启动 OpenHCL 场景的 UX**：用户先启 host 进程再启 VM；或调大 `handshake_timeout_ms`；或接受首 boot 设备缺失重启 VM。**v2 hotplug 在 §8 提升优先级**。

### 3.9 Save / Restore / Servicing

- v1：`type SavedState = SavedStateNotSupported;`（同 [vm/devices/pci/vfio_assigned_device/src/lib.rs:1274](../../../vm/devices/pci/vfio_assigned_device/src/lib.rs#L1274)）。
- **servicing 跳过点收口到单点**（v3 行号漂移已修正）：
  - 在 [openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs](../../../openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs) 的 **`create_storage_controllers_from_vtl2_settings` 内 NVMe 循环（约 L1505 附近）** 按 takeover 白名单分流：白名单内的 NVMe controller GUID 改派到 `make_pcie_remote_takeover_config(instance_id, ...)` 分支。
  - CVM 与 `is_restoring` 检查放在 **`InitialControllers::new` 入口（约 L1828）**：is_isolated → 静默过滤 takeover + CLI 注入项；is_restoring → 静默过滤所有 pcie_remote 路径 + warn。
  - 行号以提交 `87ac1a48` 为准（实施期允许 ±20 行漂移）。
- **guest 端可观察行为**：servicing 后 vpci bus 仍在，pcie_remote 设备消失。Windows vpci 驱动对设备 surprise-removal 处理是否优雅未充分验证 → spec 建议"servicing 前用户在 guest 内手动卸载该 PCIe 设备"。CLI help 与 README 写明。

### 3.10 CVM（机密 VM）：在 settings 解析阶段静默过滤；resolver 兜底 = 真造 `AbsentPcieDevice`（不 bail!）

**v3.1 修正**（v3 写"返回 sentinel"但 `AsyncResolveResource::resolve` 没有 None 变体，"静默忽略"在 Rust 不存在）：

- **第一层**（保护 boot）：在 `vtl2_settings_worker` 填充 `controllers.vpci_devices` 的最早阶段（`InitialControllers::new` 入口，§3.9），检查 `isolation.is_hardware_isolated()`。是则：
  - 过滤所有 pcie_remote 路径（takeover + CLI 注入）。
  - **不**启 vsock listener。
  - `tracing::warn!(CVM_ALLOWED, count = N, "pcie_remote skipped on CVM")`。
  - 跳过的 instance_id 列表 + 计数通过 `inspect` 暴露，**字段必须 `CVM_ALLOWED` gate**（否则 CVM 下可观测性失效）。
- **第二层**（bug 防护，不是 host-attack 防护）：resolver 内 **不 bail!**。如果 `prepared_map` 中找不到 handle（说明第一层有 bug，let through 了一条 isolated VM 的 pcie_remote handle），**真造一个 `AbsentPcieDevice` stub 返回**：

  ```rust
  /// 纯本地 stub PCI 设备，无任何 host 协议 surface。
  /// 用于：(1) CVM bug-防御；(2) 单元测试 fixture。
  /// cfg_read 全 1；cfg_write ignore；MMIO 同步返回 Err(NoResponse)。
  /// 不持 mesh::Sender / vsock pipe。
  pub struct AbsentPcieDevice { hardware_ids: HardwareIds }

  impl ChipsetDevice for AbsentPcieDevice { ... }
  // 实现按 plan F-3 修订：**不 impl GenericPciBusDevice**；pci_bus 通过
  // ChipsetDevice::supports_pci() 自动适配。实际接口见
  // vm/devices/pcie_remote_device/src/absent.rs。
  // 早期 spec 在此曾示例 impl GenericPciBusDevice，已废弃。
  ```

  Resolver 返回 `Ok(ResolvedPciDevice(Box::new(AbsentPcieDevice::new())))`。同时 `tracing::error!(CVM_ALLOWED, "BUG: pcie_remote leaked into CVM resolver; returning absent")`。
- **AbsentPcieDevice 也用于单元测试**（complete CVM 路径 + handshake_timeout 路径的回归覆盖）。

**理由**：让一次 bug 不能变成 host-triggerable boot fail；同时不需要复用 `missing_dev` resolver（其 API 是否支持 PCI 设备签名未充分验证）。

---

## 4. 代码组织

### 4.1 in-tree（**2 个**新 crate + 1 个改造）

```
vm/devices/pcie_remote_resources/   <-- 已存在；改造
  src/lib.rs                            #![forbid(unsafe_code)]
                                        现有 PcieRemoteHandle:
                                          - 标 #[deprecated(note="use PcieRemoteVmbusHandle (OpenHCL) or PcieRemoteTcpHandle (OpenVMM)")]
                                          - 保留兼容字段 socket_addr (TCP)
                                          - 保留 ResourceId "pcie_remote"
                                          - OpenVMM 端如不再用，未来删除
                                        新增 PcieRemoteTcpHandle:
                                          - ResourceId "pcie_remote_tcp"
                                          - { instance_id, socket_addr, handshake_timeout_ms }
                                          - OpenVMM 端用
                                        新增 PcieRemoteVmbusHandle:
                                          - ResourceId "pcie_remote_vmbus"
                                          - { instance_id, vsock_port, handshake_timeout_ms }
                                          - OpenHCL 端用

vm/devices/pcie_remote_protocol/    <-- 新增
  Cargo.toml                            edition.workspace, rust-version.workspace,
                                        [lints] workspace = true
                                        deps: prost, mesh, inspect
                                        build-deps: prost-build
  build.rs                              prost-build 配置：
                                          .type_attribute(".", "#[derive(mesh::MeshPayload)]")
                                          .type_attribute(".", "#[mesh(prost)]")
                                          .bytes(["."])      // Bytes 取代 Vec<u8>
                                        ⚠ 三行都不能漏
  proto/pcie_remote.proto
  src/lib.rs                            #![forbid(unsafe_code)]
                                        // 手写 codec 维持 missing_docs lint
                                        use mesh as _; use prost as _; use inspect as _;
                                        pub mod codec;   // 手写 length-prefix codec
                                        pub mod proto {  // generated 子模块单独豁免
                                            #![allow(missing_docs)]
                                            #![allow(clippy::allow_attributes)]
                                            include!(concat!(env!("OUT_DIR"), "/.../mod.rs"));
                                        }
                                        pub use proto::*;
                                        pub const MAX_FRAME_BYTES: usize = 1 << 20;
                                        pub const MAX_DMA_BYTES: usize = 64 << 10;
                                        pub const PROST_RECURSION_LIMIT: u32 = 8;

vm/devices/pcie_remote_device/      <-- 新增
  Cargo.toml                            deps: pcie_remote_protocol, pcie_remote_resources,
                                              pci_bus, pci_core, pci_resources,
                                              chipset_device, vmcore, vm_resource,
                                              pal_async, mesh, guestmem,
                                              vmsocket (support/vmsocket),
                                              async_trait, thiserror, anyhow,
                                              tracing, tracelimit, inspect, task_control,
                                              futures
                                        features: ["openhcl", "openvmm"] 控制条件编译
  src/lib.rs                            #![forbid(unsafe_code)]
                                        //! 架构说明（v3）：
                                        //!   - Resolver 自持 prepared_map，handle 仅 carry instance_id
                                        //!   - cfg 100% sync，MMIO IoResult::Defer
                                        //!   - state machine：Connecting → Live → Lost (terminal)
                                        //!   - 不支持 CVM / save_restore / hotplug（v1）
  src/transport.rs                      trait Transport: AsyncRead + AsyncWrite + ...
                                        impl 1: VsockTransport (OpenHCL)
                                        impl 2: TcpTransport (OpenVMM)
                                        + 端口黑名单校验
  src/codec.rs                          length-prefix framing on futures::io
                                          (Test 覆盖 < 4 字节读、超长帧、recursion 攻击 fixture)
  src/handshake.rs                      async fn run_listener_and_handshake<T: Transport>(
                                            transport, instance_id, handshake_timeout
                                        ) -> Result<PreparedPcieRemoteDevice, HandshakeError>
                                        含：accept 后失败不关 listener；总超时；
                                            HelloAck 校验（与 emulator assert 一一对应）
  src/prepared.rs                       struct PreparedPcieRemoteDevice {
                                            device_describe: DeviceDescribe,
                                            to_host: mesh::Sender<ToHost>,
                                            from_host: mesh::Receiver<ToOpenhcl>,
                                            // 不含 Task 也不含 listener；这些 underhill_core/openvmm 持有
                                        }
  src/resolver.rs                       AsyncResolveResource<PciDeviceHandleKind,
                                                              PcieRemoteVmbusHandle>
                                        AsyncResolveResource<PciDeviceHandleKind,
                                                              PcieRemoteTcpHandle>
                                        持 Arc<Mutex<HashMap<Guid, PreparedPcieRemoteDevice>>>
                                        resolve 不做 I/O：lock + remove + 同步组装
                                        CVM 兜底：static-resolver 创建时不可见 isolation，
                                                  所以 CVM filter 必须在调用方（underhill_core）
                                                  完成；resolver 自身只 BUG-防护：
                                                  prepared_map 中不存在 → 返回 sentinel
                                                  absent device（all-1）
  src/device.rs                         #[derive(InspectMut)]
                                        impl GenericPciBusDevice for PcieRemoteDevice
                                        持有：cfg_emu, msix, state(Arc<AtomicU8>),
                                              to_worker: mesh::Sender<DeviceRequest>
                                        含 guest data 字段 #[inspect(skip)]
                                        state ordering: Release 写、Acquire 读（CAS）
                                        ChangeDeviceState::stop -> task_control.stop().await
  src/worker.rs                         #[derive(Inspect)]
                                        async run() over Transport
                                        - 处理 ToOpenhcl 派发
                                        - 处理 DeviceRequest（薄壳侧请求）
                                        - dead-man switch（连续 N 次 + 滑窗 双条件）
                                        - 进 Lost：同步 drain in-flight + complete_error
                                        Drop = fire-and-forget Shutdown signal
                                        Graceful = task_control::TaskControl::stop().await
  src/dma.rs                            ReadGpa/WriteGpa：guest_memory 接口 + 范围 + 速率
                                        OpenHCL 路径强制 gm.vtl0()
  src/state.rs                          enum DeviceState { Connecting/Live/Lost }
                                        AtomicU8 = state.into(); 包装 read/write helper
  src/error.rs                          thiserror::Error
```

### 4.2 out-of-tree

- `pcie_remote_host_sdk`（Rust）：独立 repo。仅依赖 `pcie_remote_protocol`（git pin/vendoring）。
- 用户 host 实验程序：用户自负。
- spec 在 `docs/superpowers/examples/` 提供一个 minimal noop 设备示例（spec 自身不包含代码）。

### 4.3 接线点

**OpenHCL 端**：

1. **`vtl2_settings_worker.rs` 填充 `controllers.vpci_devices` 阶段**：
   - 在 **`InitialControllers::new` 入口（约 L1828）**：CVM 检查 → 若 isolated 静默过滤所有 pcie_remote 路径（CLI + takeover）；servicing `is_restoring` 检查 → 静默过滤 + warn。
   - 在 **`create_storage_controllers_from_vtl2_settings` NVMe 循环（约 L1505）**：按 takeover 白名单分流 —— 白名单内的 NVMe controller GUID 改派到 `make_pcie_remote_takeover_config(instance_id, ...)` 分支，构造 `UhVpciDeviceConfig { resource: PcieRemoteVmbusHandle.into_resource() }`。
   - CLI 注入路径：把 `--pcie-remote-instance` 的项加入 `vpci_devices`。
2. **underhill_core 启动期**（在装配 vpci 设备之前；接近 [worker.rs:2287](../../../openhcl/underhill_core/src/worker.rs#L2287) VfioDeviceResolver 注册点附近）：
   - 对所有筛选后的 pcie_remote 实例用 `futures::stream::FuturesUnordered` + per-instance timeout 并发跑 handshake，整体阻塞 ≤ `max(handshake_timeout_ms)`，每个失败 = warn + 跳过。
   - 构造 `PcieRemoteVmbusResolver::new(prepared_map, tasks)`。
   - **动态注册**：`resolver.add_async_resolver::<PciDeviceHandleKind, _, PcieRemoteVmbusHandle, _>(my_resolver)`。
3. **`openhcl/underhill_core/src/options.rs`** 新增 CLI：
   - `--pcie-remote-takeover <nvme_guid>:<vsock_port>[,handshake_timeout_ms=2000]`（可重复）
   - `--pcie-remote-instance <new_guid>:<vsock_port>[,handshake_timeout_ms=2000]`（可重复，要求 IGVM APPEND_CHOSEN）
   - 端口校验拒绝黑名单（§3.2）。
   - `handshake_timeout_ms` 校验 ≤ `config_timeout / 2`。
4. **不要**追加到 `openhcl/openvmm_hcl_resources/src/lib.rs` 的 `register_static_resolvers!`。该宏要求 unit struct + const，不支持持 prepared_map 的 resolver。走动态注册（步骤 2）。

**OpenVMM 端**：

1. 现有 [openvmm/openvmm_entry/src/lib.rs:610](../../../openvmm/openvmm_entry/src/lib.rs#L610) 改：构造 `PcieRemoteTcpHandle`（或 vsock 版）替代 deprecated `PcieRemoteHandle`。
2. openvmm 启动期完成 handshake（TCP listener 替换 vsock listener），动态注册 resolver：在 [openvmm/openvmm_core/src/worker/dispatch.rs:2034](../../../openvmm/openvmm_core/src/worker/dispatch.rs#L2034) VfioDeviceResolver 注册点附近 `resolver.add_async_resolver::<PciDeviceHandleKind, _, PcieRemoteTcpHandle, _>(my_resolver)`。
3. 现有 `PcieRemoteHandle.socket_addr` 兼容：v1 直接重命名为 `PcieRemoteTcpHandle`，不保留 deprecated alias（pcie_remote 当前 in-the-wild 用户为零，无兼容性成本）。

**setup.ps1**（用户运维；v3.1 补齐 ACL apply 实现）：

```powershell
# 路径：docs/superpowers/scripts/setup-pcie-remote.ps1
# 用法（管理员 PowerShell）：./setup-pcie-remote.ps1 -VsockPort 50000
param([uint32]$VsockPort)
$BaseGuid = '00000000-facb-11e6-bd58-64006a7986d3'    # HV_GUID_VSOCK_TEMPLATE
$ServiceGuid = "$($VsockPort.ToString('x8'))-$($BaseGuid.Substring(9))"
$RegPath = "HKLM:\Software\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices\$ServiceGuid"
New-Item -Path $RegPath -Force | Out-Null
Set-ItemProperty -Path $RegPath -Name 'ElementName' -Value "pcie_remote vsock:$VsockPort"

# 实际 apply ACL（v3.1 修正：v3 模板写了 TODO 注释但没真的设置 ACL）
$Sddl = 'D:P(A;;GA;;;BA)(A;;GA;;;SY)'
$Acl = Get-Acl -Path $RegPath
$Acl.SetSecurityDescriptorSddlForm($Sddl)
Set-Acl -Path $RegPath -AclObject $Acl

# 验证 inheritance flag (P = Protected，子项不继承父 ACL)
$Verify = (Get-Acl -Path $RegPath).Sddl
if ($Verify -notlike 'D:PAI*' -and $Verify -notlike 'D:P*') {
    Write-Warning "DACL not protected; check inheritance"
}
Write-Host "Registered service GUID: $ServiceGuid (ACL: Admin/SYSTEM only)"
```

---

## 5. 测试策略

| 层 | 策略 |
|----|------|
| pcie_remote_protocol::codec | rstest：帧上限、recursion、截断、字节序、超长 bytes 字段 |
| pcie_remote_protocol::proto | prost roundtrip + 跨 host SDK 互通测试（用纯 protobuf vendored） |
| pcie_remote_device::handshake | mock Transport：HelloAck 缺字段 / 非法 BAR / capability 校验失败（一一对应 emulator assert） / 超时 / failed accept 后 listener 继续 |
| pcie_remote_device::resolver | 同进程 mock：prepared_map 有 → 同步组装；prepared_map 无 → sentinel absent device；CVM mock isolation → filter 路径不调 resolver |
| pcie_remote_device::device | cfg 本地命中 / Lost 返 InvalidRegister；Defer 路径；ConfigSpaceType0Emulator 集成；access size > 8 拒绝 |
| pcie_remote_device::worker | rstest：seq 配对、in-flight 取消、超时、dead-man 连续 N 次触发、滑窗触发；Drop 与 graceful stop 双路径 |
| pcie_remote_device::dma | 范围校验、VTL2 边界、速率限制、并发请求 |
| pcie_remote_device::transport | VsockTransport（loopback fixture）+ TcpTransport |
| 端口黑名单 | 单元：每个已知端口都被拒；与 vnc_port/gdbstub_port 冲突也被拒 |
| 集成 OpenVMM | petri：spin up OpenVMM + host_stub 进程，驱动 noop PCIe 完成枚举与 reset；用 TCP loopback |
| 集成 OpenHCL（开发） | petri：OpenVMM-hosted OpenHCL，takeover 路径 + vsock loopback |
| 集成 OpenHCL（生产 Hyper-V） | 手工验证 + 文档化步骤（CI 不覆盖；setup.ps1 + PS Add-VMNvmeController） |
| 混沌 | fixture：连接抖动、慢响应、帧超限、非法 GPA、首帧非 HelloAck、恶意 dead-man 比例绕过尝试 |
| 安全回归 | mock isolation=SNP → 不起 vsock listener，takeover 与 CLI 注入都被过滤；resolver 兜底返 absent device 而非 panic |
| servicing | OpenHCL restoring → 跳过所有 pcie_remote 设备；fixture 验证 |

**覆盖率目标**：80%+。

---

## 6. 安全与合规（v3 校准）

| 风险 | v3 缓解 |
|------|---------|
| CVM 下机密内存泄露 | (1) settings 解析阶段静默过滤 (2) 不起 vsock listener (3) resolver 兜底 = 返回 sentinel absent device（不 bail!） (4) inspect 暴露被跳过的 instance |
| 通信认证 | **无应用层认证**；依赖 service GUID 注册表 ACL（用户运维 setup.ps1）；instance_id echo 仅用于运维去重，**不是认证** |
| service GUID 未注册的情况 | OpenHCL bind 失败 → warn + skip，**不 boot fail** |
| 端口冲突/撞 well-known | CLI 解析时拒绝黑名单（diag、pipette、vnc、gdbstub） |
| CLI 注入路径不可信 | spec 明写：CLI 是 unmeasured，host 完全可控；攻击面 = host 可以让某 vsock_port 起 listener，但 ACL + 端口黑名单是真正防线 |
| DMA 范围 | 仅 `gm.vtl0()`（OpenHCL）；范围校验；单次 ≤ 64 KiB；速率限制；**不宣称 IOMMU 等价** |
| 任意 MSI 注入 | 仅 `msix_index`，由 guest 已写 MSI-X 表索引；vector 低 0x20/系统向量 `MsixEmulator` 已拒 |
| vCPU 卡死 | cfg 100% 本地；MMIO Defer + 50ms 超时 |
| host hang/精准绕过 | dead-man 双条件并联：**连续 ≥4 次** + 滑窗 ≥8 次 / ≥50% |
| 首连恶意永久 wedge | failed accept 后 listener 不关，背靠总超时继续 accept |
| boot DoS（v2 bail!） | handshake 失败 / CVM filter / resolver 兜底 **全部不 boot fail** |
| prost 解析 OOM/SO | 帧 ≤ 1 MiB；DMA ≤ 64 KiB；recursion ≤ 8；每帧 1 个 oneof body |
| 日志泄露 | 含 guest 数据字段 `#[inspect(skip)]`；`tracing` 用 `CVM_ALLOWED` gate |
| servicing 路径 | `SavedStateNotSupported`；filling 期跳过 + warn；spec 建议 servicing 前 guest 手动卸载 |
| 多 VM 隔离 | vsock 按 VM ID + service GUID 二维寻址；ACL 拦截跨用户进程 |

---

## 7. 与现有代码的关系

### 7.1 必须修改

- [vm/devices/pcie_remote_resources/src/lib.rs](../../../vm/devices/pcie_remote_resources/src/lib.rs)：
  - 现有 `PcieRemoteHandle` 标 `#[deprecated]`，保留兼容
  - 新增 `PcieRemoteTcpHandle`（OpenVMM）和 `PcieRemoteVmbusHandle`（OpenHCL）
- [Cargo.toml](../../../Cargo.toml)：注册 `pcie_remote_protocol`、`pcie_remote_device` 两个新 crate
- [openhcl/underhill_core/src/options.rs](../../../openhcl/underhill_core/src/options.rs)：新增 takeover / instance CLI；端口黑名单
- [openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs](../../../openhcl/underhill_core/src/dispatch/vtl2_settings_worker.rs)：填充 `vpci_devices` 期 CVM + servicing + takeover + CLI 三阶段处理
- [openhcl/underhill_core/src/worker.rs](../../../openhcl/underhill_core/src/worker.rs)：启动期并发 handshake；prepared_map 注入；超时收口
- [openhcl/openvmm_hcl_resources/src/lib.rs](../../../openhcl/openvmm_hcl_resources/src/lib.rs)：`register_static_resolvers!` 追加 `PcieRemoteVmbusResolver`
- [openvmm/openvmm_entry/src/lib.rs](../../../openvmm/openvmm_entry/src/lib.rs)：CLI 解析改用 `PcieRemoteTcpHandle`；启动期 handshake；prepared_map
- OpenVMM 端 static resolver 注册点：追加 `PcieRemoteTcpResolver`

### 7.2 新增运维资产

- `docs/superpowers/scripts/setup-pcie-remote.ps1`：service GUID + ACL 注册
- `docs/superpowers/examples/pcie_remote_noop_host/`：host SDK 用法示例（独立 repo 也提供副本）
- `Guide/src/reference/openhcl/devices/pcie_remote.md`：用户文档（中文 + EN）

### 7.3 不修改

- [vm/devices/pci/pcie/](../../../vm/devices/pci/pcie/)
- [vm/devices/storage/nvme/](../../../vm/devices/storage/nvme/)（仅 vtl2_settings_worker 旁路 takeover GUID）
- [vm/devices/pci/vfio_assigned_device/](../../../vm/devices/pci/vfio_assigned_device/)（仅设计模板）

---

## 8. 未决项（v2 / 后续）

- **延迟接入 / hotplug**：v1 进 Lost 即 Lost，host 必须先于 boot 启动；hotplug 之后才解决"先 boot 再 connect"的体验问题。**v2 优先级 P0**。
- 共享内存 DMA（`oneof dma_transport` 已预留）
- save/restore 支持（需要重新设计 host 协议）
- dps schema 扩展（生产 Hyper-V vmwp 配合后；目前 takeover 路径足够）
- Linux guest 支持验证（vpci 主要面向 Windows guest；Linux 也支持但 spec 未在 §5 列入测试矩阵）

---

## 9. 参考源码

- vsock 双栈：[support/vmsocket/](../../../support/vmsocket/) + [openhcl/diag_server/src/lib.rs](../../../openhcl/diag_server/src/lib.rs) + [openhcl/diag_client/src/lib.rs](../../../openhcl/diag_client/src/lib.rs)
- petri 在生产 Hyper-V 用 AF_HYPERV：[petri/src/vm/hyperv/mod.rs:491](../../../petri/src/vm/hyperv/mod.rs#L491)
- 同构 90% 模板：[vm/devices/pci/vfio_assigned_device/](../../../vm/devices/pci/vfio_assigned_device/)（resolver self-owned prepared_map 范式）
- vpci 现有路径：[openhcl/underhill_core/src/vpci.rs](../../../openhcl/underhill_core/src/vpci.rs) + [vm/devices/pci/vpci/](../../../vm/devices/pci/vpci/)
- vpci OFFER 走 GET：[vm/devices/get/guest_emulation_transport/src/client.rs:585](../../../vm/devices/get/guest_emulation_transport/src/client.rs#L585)
- vpci cfg-write RMW (`now_or_never`)：[vm/devices/pci/vpci/src/bus.rs:376](../../../vm/devices/pci/vpci/src/bus.rs#L376)
- OpenHCL CLI 来自 unmeasured cmdline：[openhcl/openhcl_boot/src/host_params/dt/mod.rs:999](../../../openhcl/openhcl_boot/src/host_params/dt/mod.rs#L999)
- prost + MeshPayload build.rs：[openhcl/diag_proto/build.rs](../../../openhcl/diag_proto/build.rs)
- pci_resources：[vm/devices/pci/pci_resources/src/lib.rs:34](../../../vm/devices/pci/pci_resources/src/lib.rs#L34)
- IoResult::Defer：[vm/chipset_device/src/io.rs:30](../../../vm/chipset_device/src/io.rs#L30) + [vm/chipset_device/src/io/deferred.rs](../../../vm/chipset_device/src/io/deferred.rs)
- pci_bus deferred：[vm/devices/pci/pci_bus/src/lib.rs:507](../../../vm/devices/pci/pci_bus/src/lib.rs#L507)
- `ConfigSpaceType0Emulator` 限制：[vm/devices/pci/pci_core/src/cfg_space_emu.rs:230](../../../vm/devices/pci/pci_core/src/cfg_space_emu.rs#L230)
- `MsixEmulator`：[vm/devices/pci/pci_core/src/capabilities/msix.rs](../../../vm/devices/pci/pci_core/src/capabilities/msix.rs)
- DMA：[openhcl/openhcl_dma_manager/src/lib.rs](../../../openhcl/openhcl_dma_manager/src/lib.rs) + [openhcl/lower_vtl_permissions_guard/src/lib.rs](../../../openhcl/lower_vtl_permissions_guard/src/lib.rs)
- CVM 范本：[openhcl/openhcl_tdisp/src/lib.rs](../../../openhcl/openhcl_tdisp/src/lib.rs) + [openhcl/underhill_confidentiality/src/getters.rs](../../../openhcl/underhill_confidentiality/src/getters.rs)
- TaskControl：[vm/devices/storage/nvme/src/workers/coordinator.rs](../../../vm/devices/storage/nvme/src/workers/coordinator.rs)
- 静态 resolver 注册：[openhcl/openvmm_hcl_resources/src/lib.rs](../../../openhcl/openvmm_hcl_resources/src/lib.rs) + [vm/vmcore/vm_resource/src/lib.rs:334](../../../vm/vmcore/vm_resource/src/lib.rs#L334)
- save_restore 范本：[vm/devices/pci/vfio_assigned_device/src/lib.rs:1274](../../../vm/devices/pci/vfio_assigned_device/src/lib.rs#L1274)
- OpenVMM 现有 pcie_remote 半成品：[vm/devices/pcie_remote_resources/src/lib.rs](../../../vm/devices/pcie_remote_resources/src/lib.rs) + [openvmm/openvmm_entry/src/cli_args.rs:2490](../../../openvmm/openvmm_entry/src/cli_args.rs#L2490) + [openvmm/openvmm_entry/src/lib.rs:610](../../../openvmm/openvmm_entry/src/lib.rs#L610)

---

## 10. 已知问题与待跟进（reviewer P1/P2，不阻塞 v1 进入实施，但**必须在实施期处理**）

> 三轮 reviewer 之后剩余的非阻塞问题。每条标注**严重度 P1/P2**、**所属层（架构/Rust/安全）**、**处理时点**（v1 实施 / v1 PR review / v2）。**收手不丢弃**：任何打开 PR 时这张表都要逐条对照。

### P1：必须在 v1 实施期处理

> 状态列：✅ 已完成 / ⚠ 部分 / ❌ 未做。**P1 代码功能已全部完成（2026-05-30）。
> K-10（spec 拆分整理）是非功能性文档清理任务，保留到 v2。**

| ID | 状态 | 严重度·层 | 问题 | 处理方案 | 落地 commit |
|----|------|----------|------|----------|-------------|
| K-1 | ✅ | P1·安全 | setup.ps1 模板缺 `Set-Acl` | 同 v3.1 §4.3 模板 | 已合入 docs/superpowers/scripts/setup-pcie-remote.ps1 |
| K-2 | ✅ | P1·架构 | takeover 配置仅 CLI/env，不实现注册文件/dps | `options.rs` 加 `OPENHCL_PCIE_REMOTE_TAKEOVER` | options.rs (early commits) |
| K-3 | ✅ | P1·安全 | CLI help 文本加 WSL2 localhostForwarding 警告 | openvmm_entry/src/cli_args.rs:915 已含 | cli_args.rs |
| K-4 | ✅ | P1·架构 | path C "占位 NVMe" 真机验证 | **真 Hyper-V e2e 已通过**：N/A 占位（v1 用 instance 不用 takeover），实测 ohcldiag/vsock 全 OK | bfb4f4a3 |
| K-5 | ✅ | P1·Rust | prost `.bytes(["."])` + mesh derive 兼容性实测 | build.rs spike + diag_proto diff 一致 | 早期 commits |
| K-6 | ✅ | P1·Rust | dead-man 用 VecDeque | deadman.rs cap 16 | 早期 commits |
| K-7 | ✅ | P1·Rust | AtomicU8 Acquire/Release/AcqRel ordering | state.rs:44/54/61 | 早期 commits |
| K-8 | ✅ | P1·安全 | tracing 加 `cvm_tracing::CVM_ALLOWED` marker | handshake_spawn.rs / worker.rs / resolver.rs / options.rs 全覆盖 | f6237d43 |
| K-9 | ✅ | P1·Rust | pcie_remote_resources Cargo.toml 加 `guid` 依赖 | Cargo.toml `guid.workspace=true features=["mesh"]` | 早期 commits |
| K-10 | ⚠ | P1·架构 | spec ~700 行偏长，建议拆分 | spec 仍 758 行；附录拆分**未做**（不影响代码功能，留作 v2 整理）| — |

### P2：可在 v1 实施后/v2 前处理

> 状态列：✅ 已完成 / 🟦 v2 跟踪。

| ID | 状态 | 严重度·层 | 问题 | 处理方案 | 落地 commit |
|----|------|----------|------|----------|-------------|
| K-11 | ✅ | P2·安全 | per-instance accept 尝试次数上限 32 | `MAX_ACCEPT_ATTEMPTS=32` in handshake_spawn.rs | e5bbfac8 |
| K-12 | ✅ | P2·安全 | spec §3.7 措辞微调（不是新增 trust assumption） | spec §3.7 已更新 | 早期 commits |
| K-13 | ✅ | P2·架构 | takeover vs vmwp 未注册 GUID 行为文档化 | spec §3.1 已说明 | 早期 commits |
| K-14 | ✅ | P2·安全 | AbsentPcieDevice 纯本地 stub（不持 mesh::Sender / socket）| absent.rs 实现 | 早期 commits |
| K-15 | ✅ | P2·Rust | prost generated enum + mesh derive 兼容性 | `mesh_payload_compat` 测试 (7 tests) | f6237d43 |
| K-16 | ✅ | P2·Rust | `pub mod proto` `#[expect(missing_docs)]` | lib.rs:30-35 已加 | 早期 commits |
| K-17 | ✅ | P2·测试 | resolver duplicate 注册 panic 契约测试 | resolver.rs `add_async_resolver_duplicate_panics_contract_documented` | e5bbfac8 |
| K-18 | ✅ | P2·Rust | codec/protocol 拒绝 MMIO size ∈ {0,3,5,6,7,>8} | `is_valid_mmio_size` + size_tests + worker.rs guard | e5bbfac8 |
| K-19 | ✅ | P2·架构 | `handshake_timeout_ms ≤ config_timeout/2` 校验 + warn | options.rs `parse_pcie_remote_entries` + 2 tests | e5bbfac8 |
| K-20 | 🟦 v2 | P2·部署 | 延迟接入 / hotplug | v2 跟踪 issue（host 后启 / boot 后接入）| — |
| K-21 | 🟦 v2 | P2·测试 | path C 真 Hyper-V CI（hyperv-runner）| v2；当前只有手工 e2e（已通过，SESSION_LOG 记录）| — |
| K-22 | 🟦 v2 | P2·测试 | Linux guest 完整测试矩阵 | v2；当前 spec §5 仅 Windows guest | — |

### 处理表使用方法

- v1 PR 描述里 **逐条对照** K-1 到 K-19，标记每条的处理 commit。
- K-20 ~ K-22 在 v1 PR 合入同时开 v2 跟踪 issue。
- 任何新发现的问题追加到本表（保留编号连续）。

### v2 重构新增 K-NEW-* 项（2026-05-30 完成）

| ID | 状态 | 严重度·层 | 主旨 | 落地 |
|----|------|----------|------|------|
| K-NEW-A | ✅ | P1·安全 | MSI-X 表数据**永远不离开 OpenHCL**：MMIO 路由时 BAR4 命中走本地 `MsixEmulator::read_u32/write_u32`，绝不转发给 host | device.rs::mmio_read/write 分支 |
| K-NEW-B | ✅ | P1·架构 | host 不得占用 MSI-X 专用 BAR；DeviceBars upstream API v1 限制下 BAR1/3/5 也拒绝 | handshake.rs::validate_describe |
| K-NEW-C | ✅ | P2·安全 | DMA 累积速率限制 64 MiB/s 防止恶意 host 饱和 guest 内存带宽 | worker.rs::DmaRate (3 unit tests) |
| K-NEW-D | ✅ | P1·安全 | `cfg_write_side_effect` 仅在 `cfg_space.write_u32` 成功时 forward；失败 write 不应让 host 收到事件，否则违反 spec §3.6 "host 只看到成功的 cfg writes" 不变量 | device.rs::pci_cfg_write (rust-reviewer HIGH 修正) |
| K-NEW-E | ✅ | P2·健壮 | worker 连续 ≥4 个非法 inbound 帧 → 立即进 Lost，防止恶意 host 用 OOB msix_index / 未知 seq 等耗资源 | worker.rs::dispatch_inbound |

K-NEW-A/B/D 是 v2 重构在 BAR + MSIX + MMIO + cfg_write_side_effect 真正
接通后才浮现的新安全约束；v1 cfg-only 时这些数据通路根本不存在，故不
适用。K-NEW-C/E 是新增的健壮性约束（v2 worker 真正处理 host inbound 后
才有可能）。

### 真 Hyper-V end-to-end 验证（2026-05-30 新增）

Path C **完全闭环**：

| 验证项 | 实测证据 |
|---|---|
| OpenHCL VTL2 boot | `ohcldiag-dev inspect vm` 返回完整 tree |
| cmdline `OPENHCL_PCIE_REMOTE_INSTANCE=...` 解析 | underhill_core options.rs 实测拒绝越界 timeout (K-19 实测命中) |
| VTL2 vsock listener 启动 | `pcie_remote_device::handshake_spawn` task spawn |
| host AF_HYPERV vsock client 连入 | noop_host_vsock.exe `connected` + `received Hello magic=0x52504345` |
| §3.4 Hello/HelloAck 协议 | host 端 `received Hello version=1` + `sent HelloAck` |
| §3.3 boot grace period 超时 | 无 host 时 2.59s timeout → device absent |
| §3.10 absent fallback | timeout 后 `vsock handshake timeout; device absent` |
| handshake 成功 → worker spawn | `[2.346s] pcie_remote: vsock handshake ok, worker spawned id=11111111-...` |
| K-8 CVM_ALLOWED marker | 上述日志通过 cvm_tracing filter 仍可见 |

工件位置：详见 [HYPERV_RUNBOOK.md](../HYPERV_RUNBOOK.md) §"工件清单"。

### 真根因发现：Hyper-V VM 创建必须用 `-GuestStateIsolationType OpenHCL`

实施过程中花了相当时间才发现：用 `New-VM` 不带 `-GuestStateIsolationType OpenHCL`，
或用 petri `New-CustomVM` 创建后再修 vssd `GuestFeatureSet=0x201` + `FirmwareFile=...`，
**Hyper-V 完全静默忽略**，加载 stock Msvm UEFI，VTL2 不存在。

- 症状：ohcldiag-dev `WSA 10060 ETIMEDOUT`；`Microsoft-Windows-Hyper-V-Compute-Operational`
  event id 2000 `Create compute system, result 0xC0370103`；
  Worker-Admin event id 18605 `No bootable devices configured`
- 诊断工具：`docs/superpowers/examples/vmrs_log_scanner/`（编译后产出
  `vmrs_log_scanner_win.exe`），扫 RAM 发现只有 stock Msvm UEFI 字符串，
  无 openhcl/underhill
- 正解：`New-VM -GuestStateIsolationType OpenHCL` 在创建时指定（参 `openhcl/Set-OpenHCL-HyperV-VM.ps1`
  和 `Guide/src/user_guide/openhcl/run/hyperv.md`）

详见 [SESSION_LOG.md](../SESSION_LOG.md) "🎉🎉🎉 真 Hyper-V 端到端验证" 段。


