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
| **S6** | **QEMU-emulated-nvme 对照**（决定 fork 对不对路）| ✅ **完成（677ec341f，独立审计达成共识）**。QEMU 自带 `-device nvme,cmb_size_mb=2` 同 guest，真 oracle=`create_sq` 的 addr（非 `map_addr_cmb`，一度误用已纠）。**IO SQ 落 `0xfe000000`（CMB 内，QEMU `nvme_addr_is_cmb`→memcpy CMB backing）** ⟹ ①guest+kernel CAN（坐实）+ ②瓶颈在 vfio-user BAR-DMA 暴露链（坐实）。**③fork 对象未定**：EINVAL 可能是 QEMU client 缺 dma-buf 支持**或** firmware region_info 缺 flag（改 firmware 远省）——审计纠正过早收敛，**不该先 fork**，先做 S7 定位 |
| **S7** | **定位实验**（决定 fork 对象/是否需 fork）：guest `nvme` dyndbg/ftrace 抓 `nvme_alloc_sq_cmds`（`pci_alloc_p2pmem` NULL? vs `pci_p2pmem_virt_to_bus` 0?）+ 读 QEMU `hw/vfio-user/`/libvfio-user dma-buf 协议定位 `-22 EINVAL` 来自 client 还是 server-hint | ⏸ 待投入 |

## 净结论（截至 S6，独立审计达成共识，详见 `findings.md`）
- ✅ **firmware 侧已真机证**：CMB 广告 + 真驱动完整启用握手（CAP.CMBS→CMBMSC.CRE→CMBSZ[SQS]→CMBLOC[BIR=2]→CMSE+CBA）+ 真 NVMe IO + **p2pdma 资源注册**（64-bit prefetchable BAR 后）。in-process 测不出的真价值，已 keep。
- ✅ **S6 坐实**①guest+kernel 完全有能力 SQ-in-CMB（QEMU emulated nvme 同 guest 实证，SQE 真走 CMB backing memcpy）+ ②瓶颈在 **vfio-user 的 BAR-as-DMA-target 暴露链**（native-BAR 成功 / vfio-user-BAR `failed to create dma-buf` + map 模式同败排除假 BAR）。
- ⚠️ **fork 裁定**：反转 S5"先别 fork"——方向（动 vfio-user 这条链）对；但 **fork 对象未定**，EINVAL 可能是 QEMU client 缺 dma-buf 支持**或** firmware region_info 缺 flag（后者改几行远省）。**正确下一动作 = 低成本定位实验 S7，不是直接 fork。**

## 关联 commit
be501b07f(harness+观测性) / 568f8d037(① 64-bit BAR) / f8c1384fb(审计纠正) / 677ec341f(S6 对照+审计共识) / 本目录归档。
