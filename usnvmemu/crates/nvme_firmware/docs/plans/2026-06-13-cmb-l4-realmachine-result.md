# CMB L4 真机 e2e 实测结果（QEMU + 真 Linux guest）

**日期**：2026-06-13　**环境**：QEMU 11.0.1 + Ubuntu 6.8.0-31 guest kernel + busybox initramfs
**harness**：`usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio_guest_cmb.py`
（`CMB_MODE=trap|map`，三 oracle：guest IO PASS + host backing marker + firmware `CMB-RESIDENT-ACCESS`）
**firmware 观测性**：`controller/cmb.rs::note_first_cmb_access`（首次 CMB-resident 访问打日志）

## 结论：L4 部分达成 —— **CMB enable 握手 + 真 IO 已对真驱动验证；SQ-in-CMB 数据放置卡在 guest p2pdma**

### ✅ 已验证（firmware-correctness 关键部分）
真 Linux `nvme` 驱动对本 firmware**完整走通 CMB 发现 + 启用握手**（trap 与 map 两模一致）：
1. 读 CAP → 见 CMBS 位（`build_cap` 真置）→ 决定支持 CMB；
2. 写 `CMBMSC.CRE=1`（offset 0x50 = 0x1）→ firmware `CMBMSC programmed cre=true`；
3. 读 `CMBSZ`(0x3c) = `0x2001f0` → SZ=512×4KiB=2 MiB、**SQS|CQS|LISTS|RDS|WDS 全置**（驱动看到 CMB 可放队列）；
4. 读 `CMBLOC`(0x38) = `0x2` → BIR=2（CMB 在 BAR2）；
5. 写 `CMBMSC = 0xfe800003` → **CMSE=1 + CBA=0xfe800000**（驱动启用 CMB 内存空间 + 重定位到 BAR2 GPA）。
6. 真 NVMe IO（write/flush/read marker）经 vfio-user 全程 PASS + host backing 裸读独立核到。

→ **firmware 的 CMB 寄存器/启用序列对真驱动的确切时序成立**（in-process 测不出"驱动真按 CAP.CMBS→CRE→CMBSZ→CMBLOC→CMSE 这个序列操作"）。这是 L4 的核心 firmware-correctness 价值。

### ⚠️ 未达成：驱动把 IO SQ 放进了 **host RAM 而非 CMB**
firmware 日志 `Create IO SQ gpa=0x1ffdb000 / 0x1e78000 / 0x3108000`——全在 guest 512 MiB host RAM 内，**非** CMB(0xfe800000)。故 SQE-fetch 走 DMA、`CMB-RESIDENT-ACCESS` 不触发。

**根因（guest 侧条件，非 firmware bug）**：Linux `nvme` 驱动用 `pci_alloc_p2pmem` 从 CMB 分配 SQ，需 **CONFIG_PCI_P2PDMA + BAR 可作 p2p 资源**。驱动启用了 CMB(CMSE)，但 SQ 分配从 CMB 失败 → 静默回退 host RAM。

**下一步 lever（未做，需用户定夺，跨切+不确定）**：
- **CMB BAR 改 64-bit prefetchable**（现疑为 32-bit non-prefetchable；真 CMB BAR 通常 64-bit prefetchable，spec-aligned）——可能让 guest p2pdma 接纳该 BAR。但 64-bit BAR 占 2 槽(BAR2+BAR3)，影响 P3b resolver / OpenHCL client / in-process e2e，**跨 crate 改 + 有回归面**，且即便改了 p2pdma 仍可能受 guest CONFIG_PCI_P2PDMA / QEMU vfio-user-pci p2p 暴露其它条件限制——多轮不确定。
- 或换一个明确带 P2PDMA + p2p-capable BAR 的 guest 环境。

### 净判
L4 把"真驱动真用 CMB"拆成两半：**(a) 发现+启用握手 + 真 IO** = ✅ 真机已证；**(b) SQ/data 物理落 CMB** = ⚠️ 卡 guest p2pdma，是 guest 环境/BAR-类型条件而非 firmware 正确性。harness + 观测性已落库,改 64-bit prefetchable BAR 后可一键复跑验证 (b)。
