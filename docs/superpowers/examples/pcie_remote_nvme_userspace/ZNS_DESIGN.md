# Phase I1 (deferred) — Zoned Namespace 设计文档

NVMe ZNS Command Set Specification 1.2 — NVMe 2.0 最具标志性的新
feature。完整实现是一个完整子项目（spec ~80 页 + zone state machine +
per-zone metadata），与本 example 的"PCIe 设备模拟教学"主线 ROI 不对等。

## 实现到位需要的内容

### 数据结构

```rust
pub struct Zone {
    pub start_lba: u64,         // ZSLBA
    pub capacity: u64,           // ZCAP (≤ zone size)
    pub write_pointer: u64,      // 当前 WP，只增不减
    pub state: ZoneState,        // 状态机
    pub zone_type: u8,           // 2 = SWR (Sequential Write Required)
    pub zone_attrs: u8,          // ZA: 是否被 finished / read-only / offline
}

pub enum ZoneState {
    Empty,            // WP = start，未写过
    ImplicitOpen,     // 写过但未 explicit open
    ExplicitOpen,     // Zone Mgmt Send: Open
    Closed,           // 写后被 close 但未 finish
    Full,             // WP 已到 zone end
    ReadOnly,
    Offline,
}
```

### 新 opcode

- **Zone Mgmt Send (0x79)**：CDW12 bits 7:0 = ZSA (Zone Send Action)
  - 0x01 = Close、0x02 = Finish、0x03 = Open、0x04 = Reset、0x05 = Offline
- **Zone Mgmt Receive (0x7a)**：CDW12 bits 7:0 = ZRA (Zone Receive Action)
  - 0x00 = Report Zones（返 zone descriptor 数组）
- **Zone Append (0x7d)**：CDW10/11 = ZSLBA，controller 在 WP 处写入并
  把实际 LBA 写回 CQE.dw0

### 写规则

- **Sequential Write Required (SWR)** zone：Write SLBA 必须 == WP，否则
  SC = 0xB8 (Zone Boundary Error) 或 0xB9 (Zone Invalid Write)
- Zone Append：任意 driver 不需追 WP，controller 写到当前 WP

### Identify NS for ZNS (CSI 0x02, CNS 0x05)

返特定 4 KiB 结构：zone size / total zones / 各 limit。

### Identify Controller for ZNS (CSI 0x02, CNS 0x06)

返 zone 操作 limit。

## 决策

**保留 design doc，不实施完整 ZNS**。原因：
1. 实现量 ~1500-2000 行新代码 + state machine + 单测
2. 教学 ROI：Read/Write/Compare/Reservation 已覆盖 NVMe 核心数据流；
   ZNS 是 storage 业界 niche（SMR HDD / QLC SSD），对"理解 NVMe spec"
   不是必需
3. 真要做 ZNS demo，建议起独立 `pcie_remote_zns_userspace` example，
   复用本 SDK 而不污染当前 NVMe block 实现

## 后续如要做的步骤

1. 加 `--zns true --zone-size 256MB --zone-cap 250MB` cmd-line
2. controller 内 per-NS `Option<ZnsState>`，含 `Vec<Zone>`
3. Identify Controller CSI=0x02 暴露 Z-Cmd-Set
4. 新 opcode dispatch + 3 个 unit test
5. 真 Hyper-V e2e 用 `nvme zns report-zones`

参考实现：
- in-repo: `vm/devices/storage/nvme/src/spec/nvme/zns.rs`（OpenVMM 内
  ZNS spec struct，可直接复用）
- Linux kernel: `drivers/nvme/host/zns.c`
- nvme-cli: `nvme zns` 子命令族
