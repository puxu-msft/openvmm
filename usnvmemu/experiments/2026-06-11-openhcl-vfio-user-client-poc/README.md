# OpenHCL vfio-user client —— 可行性 POC 套件

**日期**：2026-06-11
**状态**：实验（feasibility POC，非生产代码）
**目的**：在写 `vfio_user_device`（让 OpenHCL 像 QEMU 那样以 vfio-user client 直访
guest 内存零拷贝）的设计文档**之前**，先用 POC 验证它的承重假设——避免把设计架在
未验证的猜测上（见 memory `poc-before-settling-design`）。

## POC 矩阵

| POC | 文件 | 验的承重假设 | 环境 | 结果 |
|-----|------|------------|------|------|
| 1 | `poc1_dma_zerocopy.py` | 非-QEMU vfio-user client 经 fd-passing DMA_MAP，server mmap **零拷贝双向 DMA** | 本地 | **✅ PASSED** |
| 2 | `poc2_msix_eventfd.py` | client 经 SET_IRQS 配 eventfd，firmware 完成命令后**触发 MSI-X 信号** | 本地 | **✅ PASSED** |
| 3 | `poc3_guest_ram_fd.py` | OpenHCL VTL2 经 `/dev/mshv_vtl_low` 拿**真 guest RAM** fd 并自映射 | 需真 VTL2 | ⏳ **ENV-GATED**（本地确认设备缺席符合预期；探针就绪） |
| 4 | `poc4_cvm_convert_revoke.py` | （Spec B/CVM）owner 无法单方面撤销他进程的共享映射 ⟹ 必须 revoke-before-convert | 本地（模型） | **✅ PASSED**（复现审计 CRITICAL 根因） |
| 5 | （调查，见下 §POC-5） | OpenHCL VTL2 能否承载独立 firmware 进程 + AF_UNIX | 代码调查 | ⚠️ **发现架构冲突**（见下） |

公共逻辑在 `poclib.py`（build/spawn、SCM_RIGHTS fd 传递、DMA_MAP、SET_IRQS、NVMe
bring-up），各 POC 复用；`poclib` 复用 interop_py 的 `vfio_proto` 作基础 wire。

跑：`python3 pocN_*.py`（自动 build+spawn firmware server）。并发会话正改 nvme_firmware
导致 build 红时，用 `POC_SKIP_BUILD=1 python3 pocN_*.py` 跑现成 binary。

---

## POC-1：非-QEMU client 零拷贝 DMA ✅

memfd 当 guest RAM，client 经 AF_UNIX+SCM_RIGHTS `DMA_MAP` 给 firmware，驱动 NVMe
Identify。实测：Identify 的 MN `'OpenHCL Userspace NVMe v2.0'` + CQE 零拷贝落进
**我们的 memfd**。**零拷贝自证**：全程不服务任何 server-initiated DMA_READ/WRITE，
Identify 仍完成 ⟹ server 必走 mmap（否则会阻塞等我们的 DMA reply）。

→ `vfio_user_device` 的 client 侧 DMA 路径**已证实可行**。

## POC-2：MSI-X eventfd 中断 ✅

client 经 `SET_IRQS`（`DATA_EVENTFD|ACTION_TRIGGER`，SCM_RIGHTS）配下 4 个 eventfd，
驱动 Identify，验证 firmware 完成后真 fire vector-0 eventfd（计数+1），其它向量不误触发。
**POC 抓到一个 client wire 细节**：SET_IRQS assign 必须 `DATA_EVENTFD|ACTION_TRIGGER`
二者并存（只发 DATA_EVENTFD 不 assign）——`vfio_user_device` 要写对。

→ 中断路径 **firmware→client 半段**已证实（VTL0 注入半段是 OpenHCL `Interrupt::deliver`，需真环境）。

## POC-3：真 guest RAM fd 导出 ⏳ ENV-GATED

探测 `/dev/mshv_vtl_low`（OpenHCL VTL2 内核设备）。本地（WSL2）确认缺席——符合预期，
证明这条路径是 VTL2-only 承重假设，须在**真 OpenHCL VTL2** 内跑探针（`POC_ALLOW_MSHV=1
POC_GPA=.. POC_LEN=.. python3 poc3_guest_ram_fd.py`）才能证成/证伪。**未验**。

## POC-4：CVM page-convert 撤销不变量 ✅（模型）

memfd+fork 复现审计（architect CRITICAL-2 / security #3）的根因：owner 用尽单侧手段
（drop 自己映射/msync/punch-hole）也无法让另一进程的 `MAP_SHARED` 映射失效，Linux 不给
owner 枚举/强制撤销他人映射的 API。⟹ OpenHCL `change_host_visibility` 式进程内检查对
out-of-process mmap 结构性失明 → **CVM 必须 revoke-before-convert**（sidecar 显式
munmap+ack）。证明 Spec B 的 RED 判断正确。（模型不复现硬件加密，只证根本不变量。）

## POC-5：AF_UNIX VTL2 独立进程可行性 ⚠️ 发现架构冲突

代码调查（`openhcl/underhill_core`、`openhcl/diag_server`、`vp.rs`）发现：

1. **"sidecar" 在 OpenHCL 已占用**：`sidecar_enabled()` / `spawn_sidecar_vp()` 指
   sidecar **VP**（offline CPU 的内核特性），**不是**用户态进程。我们别用这个词。
2. **underhill 是单进程模型**：所有 `spawn` 都是 `task::spawn`（async task）；**无
   `Command::new`/fork 启动独立用户态进程的先例**。
3. **AF_UNIX 在 VTL2 可用**：`diag_server` 用 `UnixListener::bind`——但那是 underhill
   自己进程内 bind，不是与另一进程通信。

**结论（必须先与用户确认，不埋进设计）**：整个"vfio-user client + AF_UNIX + SCM_RIGHTS
fd-passing"前提是 **firmware 作独立进程**。但 OpenHCL 单进程架构下，更自然的做法是把
firmware **链接进 underhill**（像所有 OpenHCL 设备一样作 crate/task）——那样它**直接持
GuestMemory**，零拷贝、根本不需要 vfio-user 协议/AF_UNIX/fd-passing。即：

> "vfio-user for OpenHCL（独立进程 + AF_UNIX）" 与 OpenHCL 单进程模型相抵；
> in-process 链接是更顺架构的零拷贝路径，但那不是 vfio-user，而是"把 controller
> 链进 underhill"（≈ 本地版 pcie_remote）。

这个 fork 改变了 Spec A 的根本形态，POC-first 在写设计前把它挖了出来。

---

## 净结论

- Spec A 的**协议/数据路径**（client fd-passing DMA 零拷贝 + MSI-X eventfd）—— POC-1/2
  **本地证实可行**，且校准了 client wire 细节。
- **真 guest RAM fd 导出**（POC-3）—— env-gated，待真 VTL2。
- **CVM**（POC-4）—— 审计 RED 判断**实测验证**，naive 直访不安全。
- **架构前提**（POC-5）—— ⚠️ 独立进程 vs in-process 链接是未决的根本 fork，
  **写 Spec A 前必须先定**。
