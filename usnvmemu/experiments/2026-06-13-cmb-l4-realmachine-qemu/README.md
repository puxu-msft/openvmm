# CMB L4 真机 e2e —— QEMU + 真 Linux guest 驱动 CMB

**日期**：2026-06-13
**类型**：真机验证实验（CMB 双模特性 §11 L4 层；in-process 测不出的"真驱动真用 CMB"）
**前置**：CMB 双模特性 P1a–P5 已 shipped（设计 `crates/nvme_firmware/docs/plans/2026-06-12-cmb-dual-mode-design.md`；in-process e2e trap+map 全绿）。本实验验**真机层**。

## 目的
真 Linux `nvme.ko` 驱动在真 QEMU 上,是否真把 IO Submission Queue(SQ)/data 放进我们 firmware 的 CMB BAR、firmware 真从 CMB backing 服务?(Linux 走 `pci_alloc_p2pmem`)。

## 环境
QEMU 11.0.1（brew）+ Ubuntu 6.8.0-31 guest kernel + busybox initramfs（缓存 `~/.cache/usnvmemu_vfio_guest/`）。

## 文件结构
- `findings.md` —— **详细分析与结论**（每阶段证据 + 独立审计纠正 + fork-QEMU 裁定）。
- harness（在其自然位置，非本目录）：`crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio_guest_cmb.py`
  （`CMB_MODE=trap|map` / `QEMU_IOMMU=1` / `GUEST_EXTRA_CMDLINE=…`；三 oracle：guest IO PASS + host backing marker + firmware `CMB-RESIDENT-ACCESS`）。
- firmware 观测性：`crates/nvme_firmware/src/controller/cmb.rs::note_first_cmb_access`（首次 CMB-resident 访问打日志）。

## 阶段性任务 + 状态
| 阶段 | 内容 | 状态 |
|---|---|---|
| S1 | 开发 L4 harness（真 guest-boot + CMB args + 三 oracle）+ firmware 观测性 | ✅ 完成（commit be501b07f）|
| S2 | trap/map 真机跑 | ✅ 完成：握手+真 IO PASS；**SQ 落 host RAM 非 CMB**（CMB-USED=False）|
| S3 | ① 64-bit prefetchable CMB BAR | ✅ 完成（568f8d037）：**让 p2pdma 资源注册成功**（dmesg `added peer-to-peer DMA memory`）；但 SQ 仍 host RAM |
| S4 | IOMMU 配置实验（intel-iommu + intel_iommu=on）| ✅ 完成：**不解**（SQ 仍非 CMB）|
| S5 | 独立 subagent 审计 | ✅ 完成（f8c1384fb）：**推翻**我"内核 p2pdma 分配限制"的错误归因；真烟枪=QEMU `failed to create dma-buf` on CMB BAR（vfio-user BAR-DMA 暴露层）；根因**仍未坐实** |
| **S6** | **QEMU-emulated-nvme 对照**（决定 fork 对不对路）| 🔄 **进行中**：QEMU 自带 `-device nvme,cmb_size_mb=2` 同 guest 跑;oracle=QEMU trace `pci_nvme_map_addr_cmb`(CMB 真被访问) + `pci_nvme_create_sq`(SQ 地址)。其 SQ 落 CMB ⟹ 洞专属 vfio-user(fork 对路);也不落 ⟹ QEMU 模拟-BAR 通病(fork 白搭) |
| S7 | 内核侧坐实（guest `nvme` dyndbg/ftrace `nvme_alloc_sq_cmds` + `/sys/.../p2pmem/`）| ⏸ 待 S6 后 |

## 净结论（截至 S5，详见 `findings.md`）
- ✅ **firmware 侧已真机证**：CMB 广告 + 真驱动完整启用握手（CAP.CMBS→CMBMSC.CRE→CMBSZ[SQS]→CMBLOC[BIR=2]→CMSE+CBA）+ 真 NVMe IO + **p2pdma 资源注册**（64-bit prefetchable BAR 后）。这是 in-process 测不出的真价值，已 keep。
- ⚠️ **SQ 物理落 CMB 未达**：卡在 **QEMU vfio-user 对 CMB BAR 的 DMA/IOMMU-mappable 暴露层**（`failed to create dma-buf`），**非**内核 p2pdma 分配层（审计据 Linux v6.8 源码纠正）。**根因待 S6/S7 坐实。**
- **fork-QEMU**：当前不该先 fork；**决策前置 = S6 对照实验**。

## 关联 commit
be501b07f(harness+观测性) / 568f8d037(① 64-bit BAR) / f8c1384fb(审计纠正) / 本目录归档。
