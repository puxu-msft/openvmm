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
| **S7** | **定位实验** | ✅ **part-2 协议侧源码溯源**：`-22 EINVAL` = QEMU client 短路（dma-buf 路径）。✅ **part-1 guest-ftrace（决定性）**：boot-kretprobe 抓 `pci_alloc_p2pmem` **命中 0 次** → guest 根本没进 p2pmem 分支（`cmb_use_sqes=false`）→ **真根因在更前一环 = firmware CMBSZ.SQS 位布局 bug**，与 dma-buf 无关。S2–S7 追错了下游 |
| **S8** | **真根因修复 + L4 完全达成** | ✅ **完成（fix 3a1779c1d）**：修两个 firmware 寄存器 bug（CMBSZ 位布局 + CMBMSC 跨 reset 生命周期，均 self-consistent trap）→ **L4 真机 trap+map 双模全 PASS**（三 oracle 一致）。**不需 fork QEMU、不需 dma-buf** |

## 净结论（S8，L4 完全达成）
- ✅ **L4 真机完全打通**：真 Linux nvme 驱动 over QEMU vfio-user，trap+map 双模把 IO SQ 放进 CMB（0xfe000000）+ firmware 从 CMB backing 取 SQE（CMB-RESIDENT-ACCESS）+ 真 write/read/flush 全 PASS（三 oracle 一致）。
- ✅ **真根因 = 两个 firmware 寄存器 bug**（非 dma-buf/fork）：① CMBSZ 位布局非 spec-aligned（SQS 编 bit4 而非 bit0）→ 真驱动判 SQS=0 拒用 CMB；② CMBMSC 跨 Controller Reset 误清 → 驱动不重编程致 cmse 丢失。两者都是 **self-consistent trap**（in-process 测试 + 误读 spec + architect review 自洽，唯真 Linux 驱动作独立 oracle 才暴露）。
- ❌ **S2–S7 的"需 fork QEMU / dma-buf"被推翻**：SQ-in-CMB 是 firmware 从**自己的** CMB backing 自读 SQE，**不经** host DMA/IOMMU/dma-buf。guest 卡在最前的 CMBSZ.SQS 判定，根本没走到 dma-buf 层。dma-buf 那条路仅对真 P2P-data（host DMA 引擎直写 CMB BAR）才相关，与 SQ/CQ/PRP-in-CMB 正交，留作未来。
- **教训**：连发两例 self-consistent trap（wire 布局 + 寄存器生命周期）；oracle 选择（症状层 `CMB-RESIDENT-ACCESS 缺失` → 根因层 guest-ftrace `pci_alloc_p2pmem 命中数`）；遇"驱动不按预期用某能力"先 ftrace 驱动**第一个**决策分支，别从最深失败处反推（S2–S7 五轮在下游 dma-buf 深挖，根因在最前一环）。

## 关联 commit
be501b07f(harness+观测性) / 568f8d037(① 64-bit BAR) / f8c1384fb(审计纠正) / 677ec341f(S6 对照) / a542a6226(S7 源码溯源) / **3a1779c1d(真根因双 bug 修复 → L4 达成)** / 本目录归档。
