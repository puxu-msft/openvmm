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
| 3 | `poc3_guest_ram_fd.py` / `poc3_mshv_vtl_low_probe.rs` | OpenHCL VTL2 独立进程经 `/dev/mshv_vtl_low` mmap **真 guest RAM** | **真 OpenHCL VM** | **✅ PASSED**（活 VM `pcie-remote-exp`）|
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

## POC-3：真 guest RAM 直访（OpenHCL VTL2 独立进程）✅ PASSED（真 VM）

两个文件：
- `poc3_guest_ram_fd.py`：本地存在性探测（本地无设备，符合预期）。
- `poc3_mshv_vtl_low_probe.rs`：静态 musl 探针 —— open `/dev/mshv_vtl_low` + 线性 mmap
  `POC_GPA` + 读写验证。

**真机实测（活 OpenHCL VM `pcie-remote-exp`，2026-06-11）**：
```
[poc3] mmap file_offset=0x100000 (GPA=0x100000)
[poc3] 读 GPA[0x100000] 首 8 字节 = 0x1c2f0006e696268   ← 真 guest 内存（非零）
[poc3] 写回 marker + 重读一致 → POC-3 PASSED ✓
```
一个**非-underhill 的独立 VTL2 进程**成功 mmap guest RAM 并双向读写。

**交付（无需重建 IGVM）**：`ohcldiag-dev run` 转发 stdin → 把静态 musl 探针经
`base64 -d` 流进活 VM 的 `/tmp` 执行：
```bash
strip /tmp/poc3_probe_static   # 静态 musl，~447KB
base64 -w0 /tmp/poc3_probe_static | \
  /mnt/c/temp/pcie_remote_exp/ohcldiag-dev.exe pcie-remote-exp run /bin/sh -- -c \
  'base64 -d >/tmp/p; chmod +x /tmp/p; POC_GPA=0x100000 POC_LEN=0x1000 /tmp/p'
```
（探针构建：`rustc --edition 2024 -O --target x86_64-unknown-linux-musl
-C target-feature=+crt-static poc3_mshv_vtl_low_probe.rs -o poc3_probe_static`）

**实测要点**：
- `/dev/mshv_vtl_low` 存在、root-only；ohcldiag-dev 起的独立进程是 root，能 open。
- 设备**只支持 mmap，不支持 read()**（`dd` 报错）——故必须 mmap 探针，不能 dd。
- 非-CVM 这台 VM：`file_offset = 裸 GPA`（无需 `SHARED_MEMORY_FLAG`）。
- **机制本就被生产代码用**（`underhill_mem/mapping.rs` 同样 open + `map_file`）；本 POC
  确认**独立进程**也能做，且实测拿到真 guest 数据。

→ **topology A（firmware-in-VTL2 零拷贝）真机证实可行。** Linux firmware binary（vfio-user
feature 已能编）放进 VTL2 即可直访 guest RAM。

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

**结论**：firmware 在 VTL2 时，"链进 underhill 进程（in-process）"与"独立 VTL2 进程"
**两种打包都可行**（独立进程有 crash/dump/vnc/gdb 先例 + diag 可 exec；in-process 则直接
持 GuestMemory）。这是 VTL2 内的打包取舍，与"firmware 在 host 还是 VTL2"（见下 Topology
总结）是正交两问。

## Topology 验证总结（firmware 该在哪 + 零拷贝机制）

**Q1：firmware 在 host 还是 VTL2？**（零拷贝直访 guest RAM 的承重前提）

| firmware 位置 | 零拷贝机制 | 结论 |
|---|---|---|
| OpenHCL **VTL2** 内 | open `/dev/mshv_vtl_low` + mmap GPA | **✅ 真机 PASSED**（POC-3，活 VM 拿到真 guest 数据）|
| **Windows host** 进程 | WHP/VID/HCS 映射外部 VM guest RAM | **❌ 不可行**（代码研究 + 对抗性 subagent 独立确认）|

host 不可行的逐机制证据（subagent 对抗性穷尽、未找到反例）：WHP `WHvCreatePartition`
只创建**自有** partition、无 open-by-id；VID/vmwp 私有不开放；HCS 仅生命周期；
membacking/virt_whp 只接**自分配** section / 自有 partition；`mshv_vtl_low` 是 **VTL2 侧**
下行设备。排除伪反例 vmrs（离线 dump）。CVM 下 host 更硬性够不到（加密）。

→ **要零拷贝，firmware 必须在 VTL2。** 现有 pcie_remote 用 host→VTL2 vsock 转发，正因
host 够不到 guest RAM（真正访问在 VTL2 侧）。

**Q2：firmware 在 VTL2 内怎么打包？** 链进 underhill（in-process，直接持 GuestMemory）
或独立 VTL2 进程（POC-5：两者都可行）——这是后续设计取舍。

## 净结论

- Spec A 的**协议/数据路径**（client fd-passing DMA 零拷贝 + MSI-X eventfd）—— POC-1/2
  **本地证实可行**，且校准了 client wire 细节。
- **真 guest RAM 直访**（POC-3）—— ✅ **真 OpenHCL VM 实测 PASSED**：独立 VTL2 进程
  mmap `/dev/mshv_vtl_low` 拿到真 guest 数据 + 写回。topology A（firmware-in-VTL2）证实可行。
- **CVM**（POC-4）—— 审计 RED 判断**实测验证**，naive 直访不安全。
- **架构前提**（POC-5）—— ✅ VTL2 内独立进程可行且有先例（underhill 已 spawn
  crash/dump/vnc/gdb；diag 可 exec）。
- **Topology**（POC-3 + B 调查）—— ✅ **已定**：firmware 必须在 **VTL2**（host 直访经
  对抗性 subagent 确认不可行）。VTL2 内打包（in-process vs 独立进程）是后续设计取舍。
