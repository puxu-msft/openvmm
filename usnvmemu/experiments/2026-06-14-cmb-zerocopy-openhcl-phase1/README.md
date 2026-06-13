# 零拷贝 CMB-on-OpenHCL Phase 1 真机 POC —— 缺口② 承重假设 A 实测

**日期**：2026-06-14  **VM**：pcie-remote-exp（真 OpenHCL Gen2，custom underhill IGVM）
**计划**：`usnvmemu/docs/superpowers/plans/2026-06-13-cmb-zerocopy-openhcl-gap-closure.md`

## 命题（承重假设 A）
host 的 `IVmGuestMemoryAccess::CreateRamGpaRange`（GET HostRequest=28）是否接受"把一段 guest GPA
（gpa_start，理想为 BAR/MMIO 空洞）别名到另一段已有 VTL0 RAM（gpa_offset）"——这是 §5 零拷贝
CMB-on-OpenHCL 路径唯一不可约的真机问题（i440bx 生产用法全是 Gen1 PAM-ROM 等址；GED stub 硬编码 FAILED）。

## 方法
custom underhill 探针（`openhcl/underhill_core/src/worker.rs` 加 `cmb_poc_probe_create_ram_gpa_range`，
未 commit，throwaway）。`cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci` →
debug/x64-custom IGVM → 部署 pcie-remote-exp → `ohcldiag-dev kmsg` 抓 STATUS。

实测内存图：VTL0 RAM `[0x0,0xf8000000)` + `[0x100000000,0x108000000)`；低位 MMIO 空洞 `[0xf8000000,0x100000000)`。

## 结果：**所有配置 FAILED=5（host 真在评估、非 blanket reject）**

iter-1（GPA 选错）：A(0x10000000 identity)=FAILED / B(0x104000000 identity)=**INVALID_GPA** / C(0x200000000 hole)=FAILED。
→ B 的 INVALID_GPA 证 host **逐个评估** error 码（不是对一切返同码）。

iter-2（真 MMIO-hole GPA + i440bx 风格 + early/late 双时序，5×2=10 探针）：**全 FAILED=5**：
| gpa_start | gpa_offset | flags/size | early | late |
|---|---|---|---|---|
| 0xf9000000(MMIO 空洞) | 0x10000000(RAM) | default | FAILED | FAILED |
| 0xf9000000 | 0x10000000 | **rom_mb**(i440bx) | FAILED | FAILED |
| 0xf9000000 | 0xf7f00000(RAM 顶,i440bx 式) | default | FAILED | FAILED |
| 0xf9100000 | 0x10000000 | 64KB | FAILED | FAILED |
| 0x10000000 | 0x10000000(identity) | default | FAILED | FAILED |

## 裁定（独立 reviewer 复核后收紧措辞）
- **承重假设 A 的"任意-空洞/RAM 别名"路径 = 本 Gen2 host 实测证伪**。host `CreateRamGpaRange` 对真 MMIO 空洞、
  RAM、identity、rom_mb 标志、i440bx 风格 gpa_offset、两种页大小、early+late 两时序——**一律 FAILED=5**。
  不是 INVALID_GPA/SLOT_OUT_OF_BOUNDS（host 接受了 GPA 和 slot），是操作本身失败。
- **🔑 排除假阴性（reviewer 强化）**：OpenVMM 树里 `CreateRamGpaRange` 的**唯一 responder = GED stub**
  （`guest_emulation_device/src/lib.rs:1031-1040`，硬编码返 FAILED、**连 INVALID_GPA 都返不出**）。而 iter-1
  观测到 **INVALID_GPA** 差异化错码 → **确证打到的是真 Hyper-V host 的 `IVmGuestMemoryAccess`、非 stub、非 API 误用**。
- **API 用法已核无误（reviewer）**：`gpa_count` 字节（同 i440bx `MemoryRange::len()`）、slot 错会返 SLOT_OOB、
  flag 错会返 INVALID_FLAG——都没收到，故 FAILED 非参数误用。全仓除 i440bx 外无第二个 create_ram_gpa_range
  成功 pattern，i440bx 也无漏掉的前置步骤，差别纯在地址落点（它在声明低 1MB RAM 内 / 我在 MMIO 空洞或异址）。
- **推翻 feasibility 实验的未验证推断**：原档称 create_ram_gpa_range "生产已证（i440bx 依赖）"——实为**从未真机测过**
  （i440bx 是 Gen1/PCAT，本 VM 是 Gen2 从不走它）。本 POC 是**首次真机调用**，结果 FAILED。"生产代码依赖" ≠ "本配置可用"。
- **唯一未测变体（标"未证伪/概念存疑"，不并进已证伪）**：gpa_start = 真实存在的设备 BAR 的 GPA（host 已知的 declared MMIO 窗口），而非任意空洞。
  i440bx 的成功 gpa_start 是 chipset 声明的 PAM-ROM 区（**在声明低 1MB RAM 内**）；host 可能只 remap 它声明过的区。本 VM `inspect` 显 `missing-pci`
  （vfio-user 设备未上线），未能取真 BAR 测。**这是 A 的最后一线生机，但概念上 host 不拥有 vfio-user BAR 的 decode。**

## 对缺口①②的影响
- **缺口②（承重）实测受阻**：§5 零拷贝路径卡在 host primitive（create_ram_gpa_range FAILED），非"只缺 wiring"。
- **缺口①（GET-backed 别名组件）moot**：primitive 不工作，建组件无意义——除非"真 BAR gpa_start"变体翻盘。
- **净**：OpenHCL 上**可用的 CMB 仍是 trap 模式**（拷贝式，QEMU L4 已 PASS）；**零拷贝 CMB-on-OpenHCL 在本 Gen2 host 实测被 create_ram_gpa_range 的 FAILED 挡住**。

## 复跑
probe 在 worker.rs（未 commit）。`cargo xflowey build-igvm x64 --override-openvmm-hcl-feature vpci` →
`cp .../debug/x64-custom/openhcl-x64-custom.bin /mnt/c/temp/pcie_remote_exp/openhcl-cmb-poc.bin` →
`configure_openhcl_boot.ps1 -VmName pcie-remote-exp -IgvmPath ...openhcl-cmb-poc.bin` → Start-VM →
`ohcldiag-dev pcie-remote-exp kmsg | Select-String 'CMB-POC'`。
