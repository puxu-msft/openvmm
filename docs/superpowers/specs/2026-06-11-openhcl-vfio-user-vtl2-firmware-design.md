# Spec A —— OpenHCL VTL2 vfio-user-like NVMe firmware（零拷贝直访 guest RAM）

> **状态**：草案 **未就绪**（2026-06-11，经 architect + security 双 audit）。承重数据路径
> 存在一个**未被任何 POC 验证的衔接点**（见 §9 audit），且安全章节 §5 不足。**不要当作可
> 转 writing-plans 的成品**——须先补 POC-6 + 按 §9 重写 §5。topology + 打包方式已定且经
> 真机验证，但"underhill 把 GuestMemory fd 传给 firmware"这一具体机制是缝合两条互斥 POC
> 路径的猜测。
>
> **非-CVM only**；CVM 另开 Spec B。POC 在
> `usnvmemu/experiments/2026-06-11-openhcl-vfio-user-client-poc/`。

## 1. 目标 & 由来

让用户态 NVMe firmware 以 **vfio-user 概念**（用户态设备后端 + 零拷贝直访 guest 内存）
服务 **OpenHCL**，替代现有 pcie_remote 的 host→VTL2 vsock **转发**模型（数据跨 vsock
拷贝、firmware 从不直接碰 guest RAM）。

**用户澄清**：vfio-user 只是概念参照，**不限 Linux 的 AF_UNIX/字面协议**——目标是
Windows/OpenHCL 上的等价零拷贝能力。

**非目标**：CVM（Spec B）；QEMU vfio-user server 路径（已 production，不动）；改动现有
pcie_remote（保留并存）。

## 2. 已验证的承重事实（POC 背书，不再是假设）

| 事实 | 证据 |
|---|---|
| **firmware 必须在 VTL2**——Windows host 进程够不到外部 Hyper-V VM 的 guest RAM | topology B 调查 + 对抗性 subagent 确认（WHP 无 open-by-id / VID 私有 / HCS 仅生命周期 / membacking 只接自有 partition）|
| VTL2 独立进程能 open `/dev/mshv_vtl_low` + mmap **真 guest RAM** 读写 | **POC-3 真 OpenHCL VM 实测 PASSED**（GPA 0x100000 拿到真数据 0x1c2f...）|
| 非-QEMU client fd-passing DMA_MAP → server mmap **零拷贝双向 DMA** | POC-1 本地 PASSED（NVMe Identify 端到端，零拷贝自证）|
| MSI-X eventfd 中断 firmware→client 信号 | POC-2 本地 PASSED |
| VTL2 能起独立用户态进程 | POC-5 代码（underhill 已 spawn crash/dump/vnc/gdb；diag 可 exec）|
| 非-CVM 此 VM：`file_offset = 裸 GPA`（无需 SHARED_MEMORY_FLAG）| POC-3 实测 |

## 3. 架构（已定：firmware 在 VTL2 独立进程 + vfio-user-like）

```
        VTL0 guest（普通 PCIe NVMe BDF，inbox nvme 驱动 0 改动）
              │ config / MMIO / DMA / MSI-X
        ┌─────┴──────────────────────────────────────────────┐
        │ OpenHCL VTL2 (underhill)                            │
        │   vfio_user_device（新，underhill 内）：             │
        │     - ChipsetDevice + PciConfigSpace + MmioIntercept │
        │       （复用 pcie_remote_device 的 PCIe 呈现层）      │
        │     - vfio-user-like client 状态机                   │
        │     - DMA_MAP：把 guest RAM fd（/dev/mshv_vtl_low 派生│
        │       或 GuestMemory mappable）经 SCM_RIGHTS 传给 fw  │
        │     - eventfd → VTL0 中断（Interrupt::deliver）       │
        └─────────────────┬───────────────────────────────────┘
              AF_UNIX      │ (vfio-user-like wire + SCM_RIGHTS fd)
        ┌──────────────────┴──────────────────────────────────┐
        │ firmware（独立 VTL2 进程，Linux ELF）：               │
        │   nvme_firmware（vfio-user feature，已能编 Linux）    │
        │   收 guest RAM fd → mmap 零拷贝 DMA（DmaBacking::Mmap）│
        │   = 现有 vfio_user_transport server，几乎零改动        │
        └──────────────────────────────────────────────────────┘
```

**关键**：firmware 侧 = 现有 `usnvmemu/crates/vfio_user_transport` server + `nvme_firmware`，
POC-1 已证它对非-QEMU client 的 fd-passing 零拷贝 DMA 可行。新工作几乎全在 underhill 侧的
`vfio_user_device`（client）。pcie_remote 保留并存。

## 4. 组件（吸收早前 architect audit）

- **`vfio_user_device`（underhill 内新设备）**：
  - PCIe 呈现**复用 pcie_remote_device 现成层**（`ConfigSpaceType0Emulator` + `MsixEmulator`
    + `Interrupt::deliver`），不重写。
  - vfio-user-like client wire（VERSION/GET_INFO/REGION_RW/SET_IRQS/DMA_MAP/RESET）。
  - AF_UNIX + SCM_RIGHTS（复用 `vhost_user_protocol::socket` 的 fd-passing 先例）。
  - DMA：从 underhill 的 `GuestMemory`（已由 `/dev/mshv_vtl_low` 线性映射 VTL0 RAM）取
    region fd，DMA_MAP 传给 firmware → firmware mmap 零拷贝（POC-1 流程）。
  - eventfd → `Interrupt::deliver` 注入 VTL0（POC-2 的 client 半段）。
- **firmware 侧**：现有 server，**仅** W0 抽 `vfio_user_wire` 共享 crate（framing 留 server）。
- **firmware 进程**：作 VTL2 独立进程跑（POC-5 路径），降权（seccomp/namespace/uid，security audit）。

## 5. 安全（非-CVM；security audit 吸收）

- **sidecar 降权 P0**：firmware 处理 untrusted guest NVMe 命令流，须 seccomp/独立 uid/namespace
  —— 补偿放弃转发模型的隔离（放弃转发 = 用户已接受 firmware 可改 guest 内存）。
- **JIT map**：仅按在飞 IO 的 PRP/SGL 窗口导出 region fd，IO 完成即 revoke（缩小暴露面）。
- **client 侧 mirror 校验**：server-initiated DMA 的 gpa+perm 由 underhill 侧独立校验
  （不信 firmware 自校验）。
- **AF_UNIX**：socket 置 paravisor-private 命名空间 + SO_PEERCRED + fd 类型校验。

## 6. 分期（W0 起）

- **W0**：抽 `vfio_user_wire`（主仓 sans-IO crate；framing 留 server；server 96 测试不破）。
- **W1**：`vfio_user_device` client wire + AF_UNIX + 握手（loopback 单测）。
- **W2**：PCIe 呈现（复用 pcie_remote 层）+ REGION_RW。
- **W3**：DMA_MAP 导出 GuestMemory region fd → firmware 零拷贝（按 POC-1）+ JIT map。
- **W4**：MSI-X eventfd → `Interrupt::deliver`（按 POC-2）。
- **W5**：firmware-as-VTL2-进程 集成 + 降权——**三段递进**部署形态（POC-3/6 真机已证 VTL2
  是完整 Linux + busybox + base64 + 可写 /tmp + ohcldiag-dev run 转发 stdin，firmware ELF
  在 VTL2 内启停/更新等同普通用户态程序生命周期，无机制障碍，约束在治理）：
  - **W5a (dev 期)**：`ohcldiag-dev run` + stdin base64 推送 ——POC-3/6 现成路径，**真"随时
    启停/更新"**，不重启 VM、不重建 IGVM；无签名/审计，仅开发迭代用。
  - **W5b (集成期)**：underhill `Command::new` 子进程 + supervisor（auto-restart + reconnect
    握手）；与 underhill 同信任域生命周期，加 seccomp/userns/uid-drop 降权。`livedump.rs` 的
    `Command::new("underhill-crash")` 是先例。**承接 §11 外部控制面：supervisor 同时是 host
    控制命令的执行点。**
  - **W5c (生产期)**：塞 IGVM initrd 出厂自带（签名链清晰、可重现），或加 A/B image 包管理；
    更新走 VM 重启。
- **W6**：underhill_core 接入 + reconnect（firmware 重启重映射/重 SET_IRQS；W5b 起依赖此）。
- **W7**：真 Hyper-V OpenHCL VM e2e（guest fio/4K IO；对照 host backing 独立 oracle）。

## 7. 测试

- W0 后 firmware 96 测试 + 真 QEMU e2e 不退。
- client ↔ firmware AF_UNIX loopback 跨进程 harness（Linux 可跑，无需 VTL）——POC-1/2 已是雏形。
- W7 真 VM e2e —— POC-3 的 ohcldiag-dev stdin+base64 投递技巧可复用作 VTL2 内启动手段。

## 8. 待 audit / 待用户审阅项

1. firmware 经 client fd-pass（DMA_MAP）拿 guest RAM vs firmware 自开 `/dev/mshv_vtl_low`
   —— 前者贴 vfio-user-like + 可 JIT/revoke + bounded；后者更直接但绕过 client 中介。本 spec
   选前者（贴用户选的 ②）。
2. firmware 进程的 VTL2 部署方式（IGVM initrd vs diag-exec 注入）—— W5 定。
3. CVM（Spec B）边界：本 spec 非-CVM；CVM 须 revoke-before-convert（POC-4）。

## 9. Audit findings（architect + security，2026-06-11）—— 转 writing-plans 前必解

**CRITICAL（两审计收敛）— 承重数据路径未被任何 POC 验证，且缝合了两条互斥路径**：
- §3/§4/§6 默认"underhill 把它的 GuestMemory region fd 经 SCM_RIGHTS 传给 firmware，
  firmware mmap 零拷贝"。但 underhill 的 GuestMemory 是**整片 `SparseMapping` over 单个
  `/dev/mshv_vtl_low` 字符设备 fd**（GPA 经 file_offset 寻址），**不是 per-region memfd**。
- POC-1 传的是 client **自造 memfd**；POC-3 是 firmware **自开 mshv_vtl_low**。**没有 POC**
  验过"underhill 把 mshv_vtl_low 派生 fd 传给另一 VTL2 进程后者按 file_offset=GPA mmap"。
- 二选一困境：传 mshv_vtl_low fd → firmware 得**整个 GPA 空间**写权（JIT/revoke 在 fd 粒度
  做不到，SCM_RIGHTS 收不回，POC-4 已证）；firmware 自开 → DMA_MAP 协议在数据路径上空转。
- **修正**：插 **POC-6**（真 VTL2 内 underhill→子进程 SCM_RIGHTS 传 fd + 子进程 mmap 真
  guest RAM）作 go/no-go 前置；据结果重写 §3/§4。**关联用户提议：可改 OpenHCL/underhill
  增加"导出 bounded、page 粒度 sub-region memfd"的 API**（见下"OpenHCL 修改实验"）。

**CRITICAL（security）— 越界 DMA 上界 + VTL 隔离弱化未论证**：
- mshv_vtl_low 给的是无边界全-GPA 写权；firmware 被攻破（NVMe parser RCE 是其本职暴露面）
  即可写 guest 任意页（含 guest kernel）甚至经构造 offset 探 VTL2/共享/特权页（confused
  deputy 弱化 VTL 隔离）。
- §5 的"client 侧 mirror 校验"对 mmap 零拷贝**结构性无效**（无 server-initiated 消息可校验）；
  JIT/revoke 依赖被攻破方配合 munmap，不可信。
- **修正**：越界上界必须由 **underhill 单方面保证**（导出 page 粒度、最小、bounded sub-region
  fd，排除 VTL2/共享/特权页），不依赖 firmware 自校验/配合 revoke。这同样指向**改 underhill
  加 bounded-export API**。

**HIGH**：
- firmware 应定性为"VTL2 内**受降权的 untrusted-facing 隔间**"，**非** paravisor 信任域成员；
  需威胁模型小节 + 区分 honest-but-buggy / compromised firmware 两模型。
- "复用 pcie_remote_device PCIe 呈现层"是**错误归因**：`ConfigSpaceType0Emulator`/`MsixEmulator`/
  `Interrupt::deliver` 来自上游 **`pci_core`**，直接依赖即可；pcie_remote 自身那层与 vsock
  worker 死耦合（defer-转发语义），不可复用。W2 按"新写"估。
- AF_UNIX 应用 **spawn-time socketpair**（不 bind 文件系统路径，杜绝第三方 connect）；
  underhill **零信任拒绝** firmware-originated SCM_RIGHTS fd。
- reconnect：导出新 fd 前必须 **SIGKILL 旧 firmware + waitpid 确认 reap**（内核保证旧映射
  全销毁）；非-CVM 双实例并存残留风险（POC-4 撤销不变量在非-CVM 同样适用，spec 误限于 CVM）。
- 分期错排：firmware-as-VTL2-进程启动编排（IGVM/initrd/拉起）是 W3/W4 真验证的**前置**，
  须提到 W2.5；W5 复杂度被低估。
- 降权（seccomp/userns/uid）能否在精简 VTL2 Linux 落地**未验**，且与"open mshv_vtl_low 高权"
  矛盾未调和 → 须 POC。

**MEDIUM**：DoS（inbound_queue 无界，需 cgroup mem limit）；pcie_remote 并存须 **per-device
路径互斥** + feature-gate；安全事件 structured-log。

**安全裁定**：非-CVM **YELLOW（近 RED 边缘）**——方向可做安全，但 §5 当前是意向声明、核心
越界边界空白、唯一真机验证路径(POC-3)给的是无边界全-GPA 写权。补齐 §9 硬门后可重审升 GREEN。

## 10. OpenHCL 修改实验track（用户 2026-06-11 提议，当前任务后做）

用户明确：**这套机制的限制若能通过改 OpenHCL 解决，当前任务完成后做相关实验**。这把多条
audit CRITICAL 从"现有 API 不可行"转为"给 underhill 加原语"：
- **bounded sub-region memfd 导出**：改 underhill，从 GuestMemory 切出 page 粒度、可
  SCM_RIGHTS 传、可单方撤销的最小窗口 fd（解 CRITICAL 越界上界 + JIT/revoke 物理可行性）。
- **host↔VTL2 共享内存**（用户另一提议）：探索经 VTL0 协商 / 在 VTL0 建 host-visible 共享
  区，让 host firmware 也能参与（复活 topology B 的一个变体）。初判：host-visible guest RAM
  （如 vmbus relay 用的）+ guest 侧协商可建**共享 bounce 区**，但①需 guest 协作（破"inbox
  driver 0 改动"）②是 bounce 拷贝非任意 guest RAM 零拷贝。须实验厘清，非显然优于 firmware-in-VTL2。
- 这些实验排在"当前 POC/spec track 收口之后"。

### 10.1 bounded 导出调研结论（2026-06-11，Explore + POC-6 真机）

- **POC-6 真机 PASS**：SCM_RIGHTS 传 mshv_vtl_low fd → 接收进程 mmap 真 guest RAM 成立
  （fd-pass 数据路径 seam 解决）。但传的是**整个设备 fd = 全 GPA 写权**。
- **bounded sub-region 零拷贝 fd 须改 HCL 内核**：mshv_vtl_low 驱动**源不在本仓**，是外部
  `microsoft/OHCL-Linux-Kernel`（flowey 拉预编译包，`resolve_openhcl_kernel_package.rs:287`）。
  需在该外部仓加 ioctl（如 `MSHV_VTL_LOW_CREATE_BOUNDED_FD(gpa,len)->fd`，mmap 强制
  `[0,len)→[gpa,gpa+len)` 越界 -EINVAL，最好实现为 dma-buf exporter）+ 重出内核包 + 本仓
  `hcl/src/ioctl.rs` 配 Rust 包装。
- **userspace-only 做不到 bounded 零拷贝**：udmabuf 不适用于 mshv 的设备/ZONE_DEVICE 页；
  已 mmap 的 VA 无法重打包成 bounded 可传 fd。userspace 唯一 bounded 路径是 **memfd bounce
  拷贝**（`sparse_mmap` 现成）——但丢零拷贝。
- **关键权衡**：用户已接受"firmware 可改 guest 内存"（topology 决策）。故**非-CVM Spec A
  用整设备 fd（POC-6 验过、纯本仓、零拷贝）功能上已足**；bounded 是 defense-in-depth 加固，
  须外部内核改动。三条路：① 整-GPA fd（本仓可落、符合已接受信任模型）② 改外部内核加 bounded
  fd（重、跨外部仓）③ bounce 拷贝（本仓但非零拷贝，自废武功）。

**决定（用户 2026-06-11）**：威胁模型已了解，**采用 ①整-GPA fd，当前不做 bounded fd**。
故外部 OHCL-Linux-Kernel 改动 track **搁置**（非阻塞、非必需）。§5 安全按"整-GPA + 进程
隔离/seccomp/socketpair/kill-reap"重写即可，不依赖内核改动。

### 10.2 host 直访 guest RAM 的完整性复核（2026-06-11，纠过宽结论 → POC 级闭环）

**纠正**："host 进程够不到 guest RAM → firmware 必须在 VTL2" 此前是**代码研究 + 对抗性
推理**得出，**未真 POC**，且过宽：WHP 无 open-by-id 是 WHP 的事实，但"任何 host 进程都够
不到"未验——vmwp.exe 本身就是映着 guest RAM 的 host 进程。

**两轮对抗性 subagent 穷尽 + 真机 POC 闭环（2026-06-11）**：

- **POC-7a（RPM-on-vmwp，真 OpenHCL VM）**：admin OpenProcess(vmwp PID 57376) OK 不被
  PPL 阻；VirtualQueryEx 遍历 1009 个 region，总 commit **仅 32.4 MiB**（远小于 GB 级
  guest RAM），扫不到 marker 0x9E66...9E66（VTL2 readback 确认 marker 仍在）→
  **OpenHCL 把 guest RAM 走 SLAT/GPA-direct，不映入 vmwp 用户态 VA**，LiveCloudKd 2010
  私有区扫描模型对 OpenHCL **不适用**。
- **POC-7b（livekd64 HvReadGpa，真 OpenHCL VM）**：`Unexpected failure from
  VidGetPartitionIds` 立即被拒——**vid.sys EPROCESS 门禁**（vid.dll 这些函数要求调用
  EPROCESS 是 vmwp，livekd 不是 → 拒）。补 `kd.exe` 后重跑同根因（KdVersionBlock 等
  失败只是 partition session 没建起来的连锁报错）。
- **两轮对抗 subagent 穷尽 12 条 host 路径**：WHP/HCS/VID/WMI/VBS/ETW/NtSystemDebug/
  `\Device\PhysicalMemory`/livekd/LiveCloudKd/驱动+WinHvReadGpa/vmbus 等**全部否定
  live 访问**（公开 API 不暴露内容；livekd/LiveCloudKd 撞 vid.sys EPROCESS 门禁或
  依赖 hvmm.sys 未文档化内核内省扒法，且 paravisor 结构未保证；自写未签名第三方驱动
  不可产品化）。
- **唯一发现的新路径**：**B1 `Save-VM → .vmrs` snapshot**（仓内 `docs/superpowers/
  examples/vmrs_log_scanner/` 已实测 OpenHCL 可行）——但**这是 frozen snapshot 不是
  live access**，对 firmware data plane 无救，只对 oracle/取证/调试有用（可作 host 侧
  独立 cross-check guest RAM 内容的工具，无需 VTL2 readback）。

**结论加固**：firmware **必须在 VTL2** 才能 live 零拷贝访问 guest RAM——已有 POC 级
（非推理级）背书。topology B 的"不可产品化"由 12 条路径全否 + 2 个真机 POC 失败实证。

## 11. host ↔ VTL2 外部控制面：`usnvmemu-ioctl`（复用 ohcldiag-dev）

**用户需求**：Windows host 控制 VTL2 内的 firmware（deploy/start/stop/update/stats/
inject/reload 等设备控制语义）。

**架构（务实选择）**：**复用 ohcldiag-dev**，扩它的 RPC schema 加 `usnvmemu-ioctl`
命令族。命名指向 usnvmemu 项目 + 强调"设备控制语义"（vfio_user 抽象层的 ioctl 模型），
不是泛 firmware-control。

```
host: ohcldiag-dev usnvmemu-ioctl <verb> [args]
       │
       └──vsock──▶ underhill diag_server
                   │  路由到 W5b supervisor
                   └──AF_UNIX──▶ firmware
```

**为什么不另起独立服务**：
- 复用现有 ohcldiag-dev 工具链 + grpc 协议 + 客户端 binary，dev 期同源工具链
- 与 W5a 的 `ohcldiag-dev run` 共享同一 vsock transport，零新 wire/0 新端口
- host 端用户只需学一个新子命令，不要装新 CLI

**命令族 `usnvmemu-ioctl` 初步动词**（随实现演进）：
- `deploy <elf>` —— 推 firmware ELF 进 VTL2 + supervisor 拉起（W5a/W5b 同语义）
- `start` / `stop` / `restart` —— 生命周期
- `stats` —— 读 firmware 性能/状态计数器
- `inject <event>` —— 测试期触发 AER / abort / 各种故障
- `reload-config` —— 重读 backing file / NS 布局等
- `reconnect` —— 显式触发 W6 reconnect

**对应 W5/W6**：W5b supervisor = `usnvmemu-ioctl` 命令的实际执行点；W6 reconnect 可由
`usnvmemu-ioctl reconnect` 或 supervisor 自检触发（同一 reconnect 流程）。
