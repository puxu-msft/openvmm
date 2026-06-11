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
| 5 | （调查，见下 §POC-5） | OpenHCL VTL2 能否承载独立 firmware 进程 + AF_UNIX | 代码调查 | ✅ **可行（有先例）** |

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

## POC-5：AF_UNIX VTL2 独立进程可行性 ✅ 可行（有先例）

代码调查（`openhcl/underhill_core`、`openhcl/diag_server`、`vp.rs`）：

1. **underhill 会起独立用户态子进程**：`livedump.rs` 用 `std::process::Command::new`
   起 `underhill-crash` / `underhill-dump`；`lib.rs` 起 vnc / gdb host 进程。**独立
   进程在 VTL2 有先例。**
2. **VTL2 能 exec 辅助二进制**：`diag_server::handle_exec` 经 `pal::unix::process::Builder`
   执行任意命令（即 `ohcldiag-dev` 的 exec 路径）——这也是把 POC-3 探针送进 VTL2 的途径。
3. **AF_UNIX 在 VTL2 可用**：`diag_server` 用 `UnixListener::bind`。
4. **命名提醒**："sidecar" 在 OpenHCL 已指 sidecar **VP**（`sidecar_enabled()`/
   `spawn_sidecar_vp()`，offline CPU 内核特性），**别用这个词**指 firmware 进程。

> **更正**：本节初版曾误判"underhill 单进程、无独立进程先例、与架构相抵"——那是第一遍
> grep 过度过滤（漏了 `livedump.rs` 的 `Command::new`）造成的错误结论。实测代码证明：
> **firmware 作 VTL2 独立进程 + AF_UNIX 可行且有先例**，并不与 OpenHCL 架构相抵。

**结论**：A（in-process 链接）vs B（独立进程 + vfio-user）**两者都可行**，不是"B 被阻塞"，
而是真实的设计取舍（in-process 最简、无协议开销 / 独立进程得统一 vfio-user 协议 + 进程隔离）。
该取舍 + 真 VTL2 的 POC-3（guest RAM fd 导出）是定 Spec A 形态前要补的两块。

---

## 净结论

- Spec A 的**协议/数据路径**（client fd-passing DMA 零拷贝 + MSI-X eventfd）—— POC-1/2
  **本地证实可行**，且校准了 client wire 细节。
- **真 guest RAM fd 导出**（POC-3）—— env-gated，待真 VTL2。
- **CVM**（POC-4）—— 审计 RED 判断**实测验证**，naive 直访不安全。
- **架构前提**（POC-5）—— ✅ 独立进程 + AF_UNIX 在 VTL2 **可行且有先例**（underhill
  已 spawn crash/dump/vnc/gdb 进程；diag 可 exec）。A（in-process）vs B（独立进程）是
  真实设计取舍、非阻塞。**写 Spec A 前需定 A/B + 补真 VTL2 的 POC-3。**
