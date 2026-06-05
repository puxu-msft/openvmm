# vfio-user Client Backend — 设计 Spec

> 在 OpenVMM 内新增 *client*-side vfio-user backend，让任意外部 vfio-user
> server（本仓 NVMe demo / SPDK / 未来其他）暴露的 PCIe 设备成为 guest 可见
> 的普通 BDF。Linux/Windows guest 用 *inbox* driver 即可消费，不写一行新
> driver 代码。
>
> 本 spec 仅做 OpenVMM (Linux host) 一侧；OpenHCL VTL2 paravisor 一侧
> **另开 spec**（"Phase B"）—— 但本设计的 crate 切分让 Phase B 至少
> 100% 复用 wire 层、部分复用 Layer 2 主体；Backend trait 大概率要重抽，
> 这点诚实声明而不假装免费复用。
>
> 状态：草稿（2026-06-05）— 待用户批准后转 `writing-plans` 出实现计划。
> 已并行经 4 路 subagent review（architect / rust-reviewer / security /
> red-team），4 个核心分歧由用户拍板：D1=A / D2=A / D3=A / D4=B。

---

## 1. 目标 & 非目标

### 1.1 目标

1. **OpenVMM host (Linux) 内新增一颗 PCIe device backend**，挂载形式与
   `vfio_assigned_device` 平起平坐，但 backing 由 `/dev/vfio/*` ioctl
   换成 UNIX socket + vfio-user wire（[Nutanix libvfio-user
   spec][spec]）。
2. Guest 看到的就是一颗普通 PCIe BDF；**Linux nvme.ko / Windows
   stornvme.sys 等 inbox driver 0 改动**即可枚举与使用。
3. 起步对端：本仓 `docs/superpowers/examples/pcie_vfio_user_sdk` 暴露的
   NVMe demo（自闭环，无外部依赖）。
4. P1 扩到 SPDK NVMe vfio-user target、Windows guest interop、反向 QEMU
   挂载验证。
5. **crate 切分一次到位**，使 Phase B（OpenHCL paravisor 一侧）至少能
   100% 复用 Layer 1 wire crate；Layer 2 / trait 形态保留调整余地。

### 1.2 非目标（本 spec 不做）

- OpenHCL VTL2 paravisor 客户端实现 — Phase B 另开 spec。
- migration / hot-plug / AER / PM cap 转发 — 推到 P2，仅在 Backend
  trait 留扩展点。
- 本仓 `pcie_vfio_user_sdk` **server 侧**任何协议补全 / 重构 — 79+
  tests 不下水。
- 写任何新 guest driver — 我们消费现有 driver。

---

## 2. 一次性问答（澄清记录）

| 问 | 答 |
|---|---|
| 客户端宿主 | 两端都支持，分两期：本 spec 做 OpenVMM；Phase B 做 OpenHCL（另 spec） |
| 第一个 interop | 本仓 NVMe server（P0）+ SPDK 矩阵（P1） |
| 教学 example 还是产品 crate | 产品级 `vm/devices/pci/vfio_user_client/` |
| 首期功能集 | P0 仅必须集 / P1 生产交付 / P2 migration 推后 |
| OS 验证矩阵 | Linux + Windows + 反向 QEMU |
| DMA 路径 | 两路都支持（fd-mmap 首选，RPC fallback） |
| Phase 划分风格 | 混合 P0/P1/P2 × W1/W2… |
| OpenHCL 二期 | 仅预留抽象，另开 spec |

## 3. 4 路 review 的 4 个核心决议（拍板结果）

| # | 决议 | 选择 | 出处 |
|---|---|---|---|
| D1 | Layer 1 现抽几个 crate？ | **A：4 crate（wire + io + client + resources），server 不重构** | rust-reviewer 强烈推；红队反对被采纳"server 不重构"那一半 |
| D2 | Backend trait 现抽几件？ | **A：5 件（mmio / irq / dma_channel / on_link_lost / io_handle），不承诺 OpenHCL 免费复用** | architect 推；红队的"OpenHCL 不可能免费复用"采纳为诚实声明 |
| D3 | P0 是否上 WRITE_MULTI / reconnect / SPDK / Win？ | **A：P0 加 WRITE_MULTI + reconnect；SPDK / Win 仍 P1** | 折中红队与原计划 |
| D4 | 安全 hardening 全前置还是分期？ | **B：仅前置最小集（F2/F5/F7），F1/F3/F6 分到 P1** | 进度优先；P0 自闭环（本仓 server）信任面小可控；接 SPDK 前必须补齐 F1+F3+F6 |

D4 的代价**显式记录**：P0 期间 OpenVMM client 只对接*受信*本仓 server，部署文档
顶部必须写明"不要拿 P0 build 接任何第三方 vfio-user server"。

---

## 4. 三层 + Backend trait 架构

### 4.1 Crate 拓扑（4 crate）

```
vm/devices/pci/
├── vfio_assigned_device/                  (已存在 — kernel VFIO passthrough)
├── vfio_user_wire/                        ← 新增 Layer 1: sans-IO wire
│   ├── deps: zerocopy + thiserror + tracing                       (NO nix / async / anyhow)
│   ├── 全平台（不加 cfg gate）— Windows host build 必须能 compile
│   ├── proto.rs     Command / Header / HeaderFlags / 各 payload struct + 严格 decode
│   ├── range.rs     newtype IoVa(u64) / Len(u64) / Perm(bits)
│   └── caps.rs      VERSION JSON + serde（deny_unknown_fields=false 兼容 SPDK）
│
├── vfio_user_io/                          ← 新增 Layer 1.5: Linux IO
│   ├── deps: vfio_user_wire + nix + libc + thiserror
│   ├── #![cfg(target_os = "linux")]
│   ├── framing.rs   UnixStream sendmsg/recvmsg + SCM_RIGHTS
│   │                 → 直接 vendor pcie_vfio_user_sdk::framing::into_owned_fd
│   │                   的 unsafe + SAFETY 注释（禁重写，对照 SDK 单测）
│   ├── transport.rs trait Transport { recv() / send_with_fds() }（sync，sans-IO 风格）
│   └── eventfd.rs   薄包装（Linux only）
│
├── vfio_user_client/                      ← 新增 Layer 2: PciDevice shim
│   ├── deps: vfio_user_wire + vfio_user_io + chipset_device + guestmem
│   │         + pci_core + vmcore + pal_async
│   ├── #![cfg(target_os = "linux")]
│   ├── lib.rs       VfioUserPciDevice impl PciConfigSpace + MmioIntercept
│   ├── backend.rs   trait Backend（5 件，见 §4.3）
│   ├── reactor.rs   pal_async worker: select(socket recv, eventfd, abort)
│   ├── region.rs    BAR shadow + REGION_READ/WRITE pump + WRITE_MULTI batching
│   ├── dma.rs       client-side DmaTable 镜像 + JIT map（P1）/ 简单 map（P0）
│   ├── irq.rs       MSI-X eventfd → MsiTarget；vector cap（F6, P1）
│   └── reset.rs     DEVICE_RESET + reconnect-on-FIN（P0-W6）
│
└── vfio_user_client_resources/            ← 新增 Layer 3 resource handle
    ├── deps: vm_resource + mesh
    └── VfioUserClientHandle { socket: PathBuf, slot: PciSlot, … }

openvmm/openvmm_entry/
├── src/cli_args.rs           +--vfio-user-client SOCKET[,bdf=…]
└── src/lib.rs                resolve VfioUserClientHandle → 与 VfioCdevDeviceHandle 同位置接入
                              (lib.rs L850-925 附近)

docs/superpowers/specs/2026-06-05-vfio-user-client-design.md   ← 本文
docs/superpowers/specs/2026-06-04-vfio-user-wire-reference.md  ← 协议字节级权威参考
```

### 4.2 Layer 边界纪律

| Layer | 允许依赖 | 禁止依赖 | 平台 |
|---|---|---|---|
| 1 wire | crates.io only (zerocopy / thiserror / tracing / serde) | nix / pal_async / guestmem / 任何 openvmm/openhcl crate | 全平台 |
| 1.5 io | + nix / libc / Layer 1 | guestmem / chipset_device / pal_async | linux-only |
| 2 client | + chipset_device / guestmem / pci_core / vmcore / pal_async | openvmm/* / openhcl/* | linux-only |
| 3 resources | + vm_resource / mesh / Layer 2 | — | linux-only |

**关键约束**：Layer 1 不 leak `memory_range::MemoryRange`；自定义
`VfioUserRange { iova: IoVa, len: Len, perm: Perm }`，Layer 2 再写
`impl From<VfioUserRange> for MemoryRange`。这保证 wire crate 公开 API
纯 std + crates.io，未来 OpenHCL Phase B 复用零负担。

### 4.3 Backend trait（5 件）

```rust
// Layer 2: vm/devices/pci/vfio_user_client/src/backend.rs
pub trait Backend: Send + Sync {
    /// guest physical address range → host accessible mapping for the server.
    /// `channel` 决定走 fd-mmap fastpath 还是 RPC（OpenHCL/CVM 可能只支持后者）。
    fn dma_channel(&self) -> DmaChannel;

    /// 主动 inject MSI-X 到 guest（vector ≤ describe num_vectors，由 caller enforce）。
    fn inject_msix(&self, vector: u16) -> Result<(), BackendError>;

    /// server-initiated DMA_READ：把 guest GPA[gpa..gpa+dst.len()] 读到 dst。
    /// 调用前 caller 已通过 client-side mirror table 校验过权限与边界（F3）。
    fn dma_read(&self, gpa: u64, dst: &mut [u8]) -> Result<(), BackendError>;
    fn dma_write(&self, gpa: u64, src: &[u8]) -> Result<(), BackendError>;

    /// socket FIN / handshake fail / RESET 后 server disconnect — Layer 3 决定怎么向 guest 表达。
    /// OpenVMM 灌 0xFF；Phase B 走 vpci revoke。
    fn on_link_lost(&self, reason: LinkLostReason);
}

pub enum DmaChannel {
    /// SCM_RIGHTS fd 推送 + server mmap（OpenVMM host 走这条）。
    /// 编译期 feature `fd-mmap` 必须启用；CVM build 关掉即在二进制层消掉 mmap 代码。
    #[cfg(feature = "fd-mmap")]
    FdMmap(Box<dyn HostMemoryMapper>),
    /// 全部 DMA 走 wire 反向 RPC（OpenHCL Phase B CVM 场景）。
    MessageMediated,
}
```

设计纪律：
- `Send + Sync` + 全 sync 方法 — 避免 `dyn`-async 坑（与 `vfio_assigned_device` 对齐）。
- `&mut [u8]` 而非 `Vec<u8>` — hot path 0 alloc。
- typed `BackendError`（thiserror）— Layer 2 是 library，不用 anyhow。
- IO reactor 不在 trait 里：reactor 由 Layer 2 持，Backend 仅被 callback。
  Phase B 若 paravisor 调度模型不同，可换 Layer 2 reactor 但 Backend
  shape 保留。
- **编译期 feature `fd-mmap` 默认 ON**（rust-reviewer R2 caveat D）：

  ```toml
  # vfio_user_io/Cargo.toml
  [features]
  default = ["fd-passing"]
  fd-passing = ["dep:nix"]   # SCM_RIGHTS + mmap 路径仅此 feature 启用

  # vfio_user_client/Cargo.toml
  [features]
  default = ["fd-mmap"]
  fd-mmap = ["vfio_user_io/fd-passing"]
  ```

  Phase B OpenHCL CVM build 关掉 default-features 即可在编译期消掉 fd
  path 代码（`into_owned_fd` unsafe 也一并消失）。§7.2 `deny_fd_path`
  runtime flag 保留为第二道闸。
- **wire crate** (`vfio_user_wire/src/lib.rs`) 必须显式
  `#![deny(unsafe_code)]`（rust-reviewer R2 caveat C1）— workspace
  lints 通常不含此项，sans-IO crate 必须单独 deny 才能闭环 finding 8。

### 4.4 server crate (`pcie_vfio_user_sdk`) 关系

- **不重构、不 re-export**。其 `proto.rs` / `framing.rs` 与 Layer 1
  形态接近但语义方向相反（server 是 receiver，client 是 sender；server
  state machine 与 client outbound build 不重合）。
- **一致性手段**：Layer 1 单测套用 server 同款 golden byte vectors，每
  改 wire 必须双向 golden 对齐；CI 跑 `cargo test -p vfio_user_wire -p
  pcie_vfio_user_sdk` 双绿。
- server 的 79 tests 不动；wire 层"共享"是字节级 + 测试级，不是代码级。
- **golden vector 单一 source of truth**（architect R2 caveat 3）：放
  `vm/devices/pci/vfio_user_wire/tests/golden/*.bin`；server crate 通过
  `path = "../vfio_user_wire/tests/golden"` 直接读，避免两侧各自
  hardcode byte literal 后 diverge。
- **CR 模板要求**（rust-reviewer R2 caveat B1 代价）：任何 wire 字段
  增删，PR 描述模板必须勾选"golden vector 双向更新 + server
  `proto.rs`/`framing.rs` 同步检查 + CI 双绿"三项。

---

## 5. Phase 表

### 5.1 W0 — crate scaffold（独立 PR，先 merge）

| | 内容 |
|---|---|
| 范围 | 4 个新 crate 空壳 + workspace 注册 + lints workspace = true + edition / rust-version inherit + linux-only gates |
| 验收 | `cargo build --workspace` 全平台绿；`cargo xtask` unused-deps 绿；79 server tests 不变 |
| 风险 | 跨平台 cfg gate 漏一个 → Windows build 红 |

### 5.2 P0 — 端到端通本仓 NVMe + Linux guest（W1…W7）

| W | 内容 | 验收门 |
|---|---|---|
| W1 | Layer 1 wire crate full + Layer 1.5 framing + VERSION/handshake + GET_INFO/GET_REGION_INFO | unit + socketpair fixture：client ↔ test server handshake 成功 |
| W2 | REGION_READ/WRITE pump + cfg space + BAR0 shadow + 接入 `chipset_device`。**MMIO handler 返 `IoResult::Defer`，由 Layer 2 reactor 在 socket reply 到达时 complete deferred token；不允许同步阻塞 in-VP-thread**（复用 `pcie_remote_device` 已验证模式 — architect R2 caveat 1） | guest UEFI / Linux kernel 看到 NVMe BDF（lspci 列出）+ 非对齐 access (1/2/4/8B) 拆/合策略对齐 `vfio_assigned_device` 现有实现 |
| W3 | DMA_MAP/UNMAP（**简单模式**：启动时一次性 map guest RAM；F1 JIT map 推到 P1）。**wire-level decode 必须能解析 `DMA_UNMAP + GET_DIRTY_BITMAP` flag 并合法回 reply**（即使 client 不真做 dirty 跟踪，回全 0 bitmap 也行 — architect R2 caveat 2，避免 P1 W8 上线时返工 wire crate） | nvme.ko Identify Controller 通过 |
| W4 | SET_IRQS eventfd + Backend::inject_msix（**vector cap 推到 P1 F6**） | nvme.ko Create IO Queue + 中断到达 |
| W5 | DEVICE_RESET 转发 + 本地短路 CSTS.RDY。**显式 KNOWN-BAD：本地短路 CSTS.RDY 仅覆盖 Linux nvme.ko unload/reload；Windows stornvme.sys 在 reset 后会走 REPORT LUNS / INQUIRY / sense buffer 查询，需要 W12 完整实现，P0 不验 Windows reset 路径** | nvme.ko unload/reload 通过；spec doc + W5 任务卡显式列 Windows reset retrofit 待 W12 |
| W6 | **reconnect-on-FIN**：socket close → 灌 0xFF + log，**P0 实装不留 P2**。区分 `recv() == 0` (FIN) vs `EPIPE` (write 时半关) vs `SHUT_WR` (half-shutdown) 三种触发，记录到任务卡 | 杀掉 server 进程后 guest 看到 device gone（lspci `--` 消失），重启 server 不自动重连（P1 W11 才做）；**新增单测**：server 在 DMA in-flight 时 close socket，client 必须取消未完成 DMA（不悬挂、不 panic、不读到部分写入的 dst buffer） |
| W7 | **REGION_WRITE_MULTI batching**（接 SPDK 必备，提前到 P0） | **socketpair fixture 内 mock server 发 ≥2 条 multi-write，client BAR shadow 正确合并**（不再只验 codec — red-team R2 caveat 1） |
| **P0 验收** | 本仓 server 自闭环。**禁止**接第三方 server（D4 trade-off — 由 CLI 默认拒绝非白名单 socket 路径技术 enforce，见 §7.1）。fio 验收（red-team R2 caveat D — 单 randread 30s 是误绿门）：<br>① `fio --rw=randread --bs=4k --iodepth=32 --numjobs=4 --runtime=30s` 0 错<br>② `fio --rw=randwrite --bs=4k --iodepth=32 --numjobs=4 --runtime=30s` 0 错<br>③ `fio --rw=randrw --rwmixwrite=50 --bs=4k --iodepth=32 --numjobs=4 --runtime=60s` 0 错<br>④ guest 内强制 NVMe BAR0 重定位到 >4GiB（OVMF 配置或重映射）后重跑 ① 30s — 0 错（覆盖 64-bit BAR 路径） | |

P0 期间最小安全前置（D4=B），由 security R2 caveat 加固：
- **F2** REGION reply payload 长度 `len == count` 严格校验（W2 入口）
- **F4-baseline** 提前到 P0 W1：SO_PEERCRED 取 peer uid/gid/pid + fd type validation（SCM_RIGHTS 收到 fd 经 `fstatfs`/`fcntl` 校验类型）+ socket parent dir 校验（owner == euid && mode & 0o022 == 0）
- **F5** 复用 SDK `into_owned_fd` SAFETY 注释（W1 借由 vendor）
- **F7** `SO_RCVTIMEO/SNDTIMEO` + handshake 5s timeout（W1）
- **F8-基础** malformed message → close socket + `catch_unwind` 边界包裹 inbound 命令处理（W1）

### 5.3 P1 — 生产交付（W8…W13）

| W | 内容 | 验收门 |
|---|---|---|
| W8 | **F1 JIT map**：W3 启动一次性 map → 改 NVMe IO 触发的 just-in-time MAP/UNMAP。**UNMAP reply 必须能合法响应 GET_DIRTY_BITMAP flag**（已在 P0 W3 准备 wire 解码；W8 落实生成空 bitmap reply 路径） | fio 通；map 窗口 ≤ MDTS；DMA 完成立即 UNMAP；**SLA：max in-flight unmapped GPA pages ≤ MDTS × max_qd × num_queues × 安全系数 2**（security R2 caveat 3，单测验证）；单测验证 server 不能看到非活动 IO 的页 |
| W9 | **F3 client-side mirror region table** + DMA RPC fallback 路径完整 | mock 恶意 server 试图越界 DMA → client reject |
| W10 | **F6 vector cap + token bucket** + **F9 MAX_MSG_FDS=16 + 进程级 fd 计数 + RLIMIT_NOFILE 启动 cap** + INTx/MSI（非 MSI-X）支持 | fuzz vector 0..2^32 / 风暴注入 → client 拦截；fd 计数器在恶意 server 灌满 fd 时触发 RLIMIT 拒收 |
| W11 | **reconnect 自动重连**：socket 重建 + 重发 DMA_MAP + 重协商。区分 FIN / EPIPE / SHUT_WR 三路触发；**包含 circuit breaker：N 次连续失败后停止自动重连，等管理面介入** | 杀 server + 重启 server，guest IO 暂停后恢复（带状态告警） |
| W12 | Windows guest interop（stornvme.sys + DISKSPD）+ **完整 reset 路径**（W5 仅本地短路 + Linux nvme.ko；W12 补 stornvme 在 reset 后的 REPORT LUNS / INQUIRY / sense buffer 走 IO 队列的 admin 重建路径） | Windows IOPS baseline 不低于 Linux 90%；reset/load/unload 在 Windows event log 无 disk error |
| W13 | SPDK NVMe vfio-user target interop + 反向 QEMU 挂本仓 server 验证 wire 双向。**SPDK-specific 验证项**：① max_data_xfer_size=1MiB 单次 DMA 通；② GET_REGION_IO_FDS 命令 client 返 ENOTSUP 后 SPDK 不断连；③ PCIe ext caps (AER/PASID/PRI/ATS) filter 策略对齐 `vfio_assigned_device::parse_extended_capabilities` | interop matrix（{Linux/Win} × {本仓/SPDK} × {fd-mmap/RPC}）≥ 90% GREEN |
| **P1 验收** | interop matrix 报告 + 文档化已知 SPDK 差异 + 安全 audit 通过（F1-F9 全做）。**Hard gate（security R2 caveat 4）**：F1+F3+F6 必须在 W13 SPDK interop 首次执行**之前**完成 merge & security-reviewer signed-off；W13 不得以 hardening 未完为由跳过 | |

### 5.4 P2（推后，本 spec 仅占位）

- DEVICE_FEATURE migration / MIG_DATA_RW
- hot-plug / vpci 触发
- AER cap / PM cap forward
- 另开 spec。

---

## 6. 安全模型 & 信任面

### 6.1 信任假设

```
Guest VTL0          UNTRUSTED
   │  MMIO/cfg
   ▼
OpenVMM host        TCB (本设计代码所在)
   │  UNIX socket + SCM_RIGHTS
   ▼
vfio-user server    SEMI-TRUSTED (P0=信任本仓，P1+=允许 SPDK 等第三方)
   │
   ▼ via fd-mmap or RPC
Guest physical RAM  UNTRUSTED data / TRUSTED layout
```

**核心错位**：vfio-user 协议本身**无 IOMMU 抽象**。server 一旦拿到
DMA_MAP region，对该 GPA 段 R/W 不受协议约束。这意味着：

- P0 只能对接*受信* server（本仓 demo）。部署文档顶部明示。
- P1 接 SPDK 前必须完成 F1（JIT map 收窗口）+ F3（镜像 table）+ F6
  （vector cap）三项 hardening；任一缺失 = 高危绕过。

### 6.2 必须的安全 check 清单（与 Phase 绑定）

| ID | check | Phase |
|---|---|---|
| F2 | REGION reply payload `len == count` 严格匹配 | P0 W2 |
| F5 | SCM_RIGHTS fd 包装复用 SDK SAFETY，禁止重写 | P0 W1 |
| F7 | socket SO_*TIMEO + handshake 5s + per-request deadline + circuit breaker | P0 W1 + W11 增强 |
| F1 | JIT DMA_MAP（不一次性 map 整 guest RAM） | P1 W8 |
| F3 | client-side mirror DmaTable 校验 server-initiated DMA gpa+perm | P1 W9 |
| F6 | MSI-X vector ≤ describe num_vectors + per-vector token bucket | P1 W10 |
| F4-baseline | SO_PEERCRED + fd type validation + socket parent dir mode 检查 | **P0 W1**（security R2 caveat 1） |
| F4-full | socket path TOCTOU 完整防御（mkdir + bind + chmod 原子序列） | P1 W13 |
| F8 | malformed message → close socket + catch_unwind 边界 | P0 W1（基础）+ P2 fuzz harness |
| F9 | MAX_MSG_FDS=16 + 进程级 fd 计数 + RLIMIT_NOFILE | P1 W10 |
| F10 | CLI 暴露 → 文档化 ENV/config-file 替代路径 + `--help` 含"DO NOT pass socket path via shell history"提示 | P0 W0（CLI 提示）+ P2（ENV/config 替代） |

### 6.3 部署 README 必含警告

```
# WARNING — vfio-user trust model

A vfio-user server process has UNCONSTRAINED R/W access to all guest
physical memory pages that this client DMA_MAPs to it. In effect, the
server is INSIDE your hypervisor TCB.

DO NOT connect this backend to any vfio-user server you do not own
end-to-end, including:
  - SPDK builds you did not build & audit yourself (P1 phase only)
  - Any socket path under world-writable parent directories
  - Any server running with different effective uid than this client

P0 builds are validated ONLY against `pcie_vfio_user_sdk` demo server
in-tree. Production deployments must wait for P1 completion (F1+F3+F6).
```

---

## 7. 接入点（OpenVMM CLI + resolver）

### 7.1 CLI 形态

```
openvmm \
    --vfio-user-client /run/openvmm/trusted/foo.sock,bdf=0000:00:08.0 \
    --vfio-user-client /run/openvmm/trusted/bar.sock,bdf=0000:00:09.0 \
    [--allow-untrusted-vfio-user]   # 显式 opt-in 才能用白名单外路径
    [其它选项...]
```

`--vfio-user-client SOCKET[,bdf=BDF]`，与
`--vfio-cdev` / `--vfio` 同位置注册（`openvmm_entry/src/lib.rs` L850-925
附近）。一次可重复多次。

**默认白名单 enforce**（security R2 caveat 2，把 D4=B 的"禁止接第三方
server"从纯文档升级为技术约束）：
- 默认只接受 `socket_path` 在 `/run/openvmm/trusted/*.sock` 下
- 越出白名单需要显式 `--allow-untrusted-vfio-user` flag
- 命中后 stderr 一次性打 RED warning：
  ```
  WARNING: --allow-untrusted-vfio-user enables connections to socket
  paths outside /run/openvmm/trusted/. The peer process gains FULL R/W
  access to guest memory via DMA_MAP. Only enable if you own & audit
  the server end-to-end.
  ```
- `--help` 文本含一行："SOCKET path will appear in /proc/<pid>/cmdline;
  prefer ENV var or config file (P2) for stricter deployments."（F10
  P0 部分）

### 7.2 Resource handle

```rust
// vfio_user_client_resources
pub struct VfioUserClientHandle {
    pub socket_path: PathBuf,
    pub pci_slot: PciSlot,
    pub deny_fd_path: bool,          // 强制 RPC（CVM-friendly），默认 false
    pub max_data_xfer_size: usize,   // 默认与 server caps 协商后取小
}
impl_resource_for!(VfioUserClientHandle, PciDeviceHandleKind);
```

### 7.3 resolver

`vfio_user_client::resolver` 实现 `ResolveResource<PciDeviceHandleKind,
VfioUserClientHandle>`，返回 `Box<dyn PciDevice>`，与
`vfio_assigned_device::resolver` 形态对齐（参考 lib.rs L867-925）。

---

## 8. Phase B（OpenHCL）— 仅承诺与边界

本 spec **不写** OpenHCL 代码。在 crate 切分层做这些承诺：

| 承诺 | 兑现机制 |
|---|---|
| Layer 1 wire crate 100% 复用 | 全平台 sans-IO + 无 OpenVMM 依赖 |
| Layer 1.5 io crate 大概率复用 | linux-only 但无 VMM 依赖；OpenHCL 也跑 Linux paravisor |
| Layer 2 reactor / region pump / WRITE_MULTI batching 复用 | Backend trait 抽象足够 |
| Backend trait 5 件**可能**够用 | 已包含 dma_channel 枚举 + on_link_lost 钩子 |
| **不承诺** Backend trait 形态不动 | OpenHCL 真做时若发现缺第 6 件（CVM visibility / vpci-revoke 等），改 trait 是正常 phase B 工作 |

红队的"先做 OpenHCL → 反向决定 trait"被采纳为"不假装免费复用"的诚实
声明，但**不**强制等 OpenHCL 完工才动 OpenVMM；商业上 OpenVMM 是 P0
价值交付点。

---

## 9. 风险登记册（按概率 × 影响排序）

| # | 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| R1 | MSI-X coalescing / vector routing 错位（red-team P0 失败点 #1） | 高 | 高 | P0 W4 单测：mock server 触发任意 vec → guest 端验证 routing；接入既有 `MsixEmulator` 自检 |
| R2 | DMA fence / 顺序（fd-mmap fastpath 一致性） | 中 | 高 | OpenVMM host P0 默认走 `DmaChannel::FdMmap`（性能首选，与 §4.3 一致）；fd-mmap 路径明文要求：server 端 mmap region 后第一次写前依赖 PCIe ordering，由 client 端文档化禁止 `MAP_POPULATE` 之外的 stale-page lazy fault 行为；可疑场景加 `msync(MS_SYNC)` 兜底（red-team R2 caveat 3） |
| R3 | SPDK 发 REGION_WRITE_MULTI / max_xfer 1MiB / GET_REGION_IO_FDS / AER ext caps 把 P0 demo 打挂 | 高 | 中 | WRITE_MULTI 已提到 P0 W7；其余三项在 P1 W13 SPDK 任务卡专项验证 + ext caps filter 对齐 `vfio_assigned_device::parse_extended_capabilities` |
| R4 | reset timing 在 Windows 上 BSOD | 中 | 高 | W5 本地 CSTS.RDY 短路（Linux 路径），KNOWN-BAD 显式标记 Windows 留 W12；W12 完整实现 stornvme reset 序列 |
| R5 | Layer 1 wire 与 server 字节定义 diverge | 中 | 高 | 双向 golden vector 单一 source-of-truth (`vfio_user_wire/tests/golden/`) + CI 双绿 gate + CR 模板勾选项（rust-reviewer R2 caveat B3：概率由"低"升"中"，长期两份代码 diverge 是经验事件） |
| R6 | OpenVMM Windows host build 因新 crate 红 | 中 | 中 | Layer 1 跨平台 + W0 CI 加 Windows build smoke |
| R7 | server compromise → guest RAM 全面泄露 | 低（P0 仅本仓 server 且白名单 enforce） / 中（P1 SPDK） | 致命 | P0 README 警告 + CLI 默认白名单 + P1 F1+F3+F6 完整 hardening |
| R8 | SELinux/AppArmor profile 缺失 | 中 | 中 | P2 决策项；plan 任务卡含"我们是否发 profile"决议 |
| R9 | NUMA 亲和缺失导致 SPDK interop 性能退化 | 中 | 中 | P1 W13 实验时记录 client/server 不同 NUMA node IOPS 退化数据 |

---

## 10. 验收 & 退出标准

- **W0 退出**：4 新 crate 全平台 build 绿；79 server tests 不变；workspace lint 全绿；`vfio_user_wire/src/lib.rs` 含 `#![deny(unsafe_code)]`；CLI `--help` 含 socket path 警告行（F10 P0 部分）。
- **P0 退出**：F2 / F4-baseline / F5 / F7 / F8-基础 hardening 落地；本 spec 自带 smoke test bin（参考 `nvme_of_tcp_target` 风格）；fio 4 项门全绿（见 §5.2 P0 验收）；CLI 白名单 enforce 工作（mock 非白名单路径 → 默认 reject）。
- **P1 退出**：interop matrix（{Linux, Win} × {本仓, SPDK} × {fd-mmap, RPC}）覆盖 ≥ 90% GREEN；F1+F3+F4-full+F6+F9 hardening 落地；安全 audit 报告归档；**Hard gate**：F1+F3+F6 必须在 W13 SPDK interop 首次执行*之前*完成 merge + security-reviewer signed-off（security R2 caveat 4）。
- **P2 开门条件**：另开 spec；不阻塞 P0/P1 合并。

---

## 11. Plan-stage Open Issues（writing-plans 必须吸收）

来自第二轮 review 的非 spec-blocking caveat，必须写入 plan 头部：

- **A1**（architect B.3）：plan 指定 golden vector 物理位置 `vm/devices/pci/vfio_user_wire/tests/golden/*.bin`；server crate `path = "../vfio_user_wire/tests/golden"` 共享读取。
- **R1**（rust-reviewer C2）：PR 模板加"wire 字段增删 → golden vector 双向 + server proto 同步"勾选项。
- **R2-a**（red-team B2-a）：SELinux/AppArmor profile 决策（发 / 不发）。
- **R2-b**（red-team B2-b）：P1 W13 SPDK NUMA 亲和退化数据采集。
- **R2-c**（red-team B2-c）：W6 reconnect trigger 三路触发明确（FIN / EPIPE / SHUT_WR）。
- **R2-d**（red-team B2-d）：max_data_xfer_size 协商失败 fallback 决策。
- **R2-e**（red-team B2-e）：W13 验证 GET_REGION_IO_FDS 返 ENOTSUP 不断连。
- **R2-f**（red-team B2-f）：W2 BAR shadow 非自然对齐访问拆/合策略对齐 `vfio_assigned_device`。

---

## 12. 已批准 / 待用户审阅项

1. ✅ CLI 形态：`--vfio-user-client SOCKET[,bdf=BDF]` + 默认白名单 + `--allow-untrusted-vfio-user` opt-in（security R2 caveat 2）
2. ✅ resource handle 字段：`socket_path` / `pci_slot` / `deny_fd_path` / `max_data_xfer_size`
3. **待审**：P0 fio 时长 — §5.2 已升级为 4 个 30-60s 多模式 fio。要不要其中一项拉长到 300s 看长期稳定？
4. ✅ README 警告 + CLI 默认白名单 — D4=B 的"禁止接第三方"由文档 + 技术双层 enforce

---

## 13. Round-2 Review 收敛记录

第二轮 4 路独立 review 全部给出 **YES with caveats**。本 spec 已合并所有
"必须改 spec"项：

| 来源 | caveat | 落地位置 |
|---|---|---|
| architect B.1 | W2 MMIO 必须 `IoResult::Defer` | §5.2 W2 |
| architect B.2 | W3 wire 必须解 `DMA_UNMAP + DIRTY_BITMAP` flag | §5.2 W3 + §5.3 W8 |
| architect B.3 | golden vector 单一 source-of-truth 路径 | §4.4 |
| rust-reviewer C1 | wire crate `#![deny(unsafe_code)]` 显式 | §4.3 + §10 W0 |
| rust-reviewer C2 | PR 模板 wire 同步勾选项 | §4.4 + §11 R1 |
| rust-reviewer D | `fd-mmap` 编译期 feature | §4.3 |
| security 1 | F4-baseline 提前 P0 W1 | §5.2 + §6.2 |
| security 2 | CLI 白名单 + `--allow-untrusted-vfio-user` | §7.1 + §10 P0 |
| security 3 | W8 JIT map SLA 量化 | §5.3 W8 |
| security 4 | F1+F3+F6 必须在 W13 之前 signed-off | §5.3 P1 + §10 P1 |
| security 5 | W6 FIN-during-DMA 单测 | §5.2 W6 |
| red-team B1-a | W7 mock server 真发 WRITE_MULTI | §5.2 W7 |
| red-team B1-b | W5 Windows reset KNOWN-BAD 显式 + W12 完整 | §5.2 W5 + §5.3 W12 |
| red-team B1-c | R2 与 §4.3 `FdMmap` 默认路径口径统一 | §9 R2 |
| red-team D | P0 fio 验收门补 3 行（randwrite/randrw/64-bit BAR） | §5.2 P0 验收 |

剩余 plan-stage open issues 列在 §11。

---

[spec]: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst
