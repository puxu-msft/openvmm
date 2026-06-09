// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Get Log Page 序列化器（spec § 5.16）。
//!
//! Phase J 模块化重构：原本散落在 `controller/mod.rs` 内的 5 个
//! `build_*_log` 方法集中到此处，每个 builder 都是纯函数：输入
//! `&NvmeController` + 请求字节数，输出 4 KiB 以内的 byte buffer。
//!
//! 这样让 mod.rs 专注 state machine + dispatch，log 序列化的 spec
//! offset 都在一处便于对照 spec § 5.16.1.x 文档审计。

use super::NvmeController;

/// **Log Page 0x01** — Error Information Log (spec § 5.16.1.1)。
///
/// 每个 entry 64 字节，最多 ELPE+1 = 64 entry。spec 要求 entry[0] =
/// 最近一次。我们的 `error_log` VecDeque 按 push 顺序（旧→新）保存，
/// build 时 `.rev()` 倒序写。
pub(super) fn build_error_info(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes];
    for (i, e) in c.error_log.iter().rev().enumerate() {
        let off = i * 64;
        if off + 64 > bytes {
            break;
        }
        buf[off..off + 8].copy_from_slice(&e.error_count.to_le_bytes());
        buf[off + 8..off + 10].copy_from_slice(&e.sq_id.to_le_bytes());
        buf[off + 10..off + 12].copy_from_slice(&e.cid.to_le_bytes());
        buf[off + 12..off + 14].copy_from_slice(&e.status_field.to_le_bytes());
        buf[off + 14..off + 16].copy_from_slice(&e.param_loc.to_le_bytes());
        buf[off + 16..off + 24].copy_from_slice(&e.lba.to_le_bytes());
        buf[off + 24..off + 28].copy_from_slice(&e.nsid.to_le_bytes());
        // offset 28..64 = vendor info / log page ver / cmd-specific = 0
    }
    buf
}

/// **Log Page 0x02** — SMART / Health Information (spec § 5.16.1.2, 512 字节)。
///
/// Phase F 真追踪 counters；Phase G reviewer H3 修复用 `div_ceil(1000)` round-up
/// （spec 'rounded up'）。
pub(super) fn build_smart_health(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    // Offset 0: critical_warning (1 byte) = 0
    // Offset 1-2: composite_temperature (2 byte LE, in Kelvin)
    let temp_kelvin: u16 = 313;
    buf[1..3].copy_from_slice(&temp_kelvin.to_le_bytes());
    buf[3] = 100; // available_spare
    buf[4] = 10; // available_spare_threshold
    buf[5] = 0; // percentage_used
    let units_read = c.stat_lba_read.div_ceil(1000) as u128;
    let units_written = c.stat_lba_written.div_ceil(1000) as u128;
    let host_reads = c.stat_host_reads as u128;
    let host_writes = c.stat_host_writes as u128;
    buf[32..48].copy_from_slice(&units_read.to_le_bytes());
    buf[48..64].copy_from_slice(&units_written.to_le_bytes());
    buf[64..80].copy_from_slice(&host_reads.to_le_bytes());
    buf[80..96].copy_from_slice(&host_writes.to_le_bytes());
    let one: u128 = 1;
    buf[112..128].copy_from_slice(&one.to_le_bytes()); // power_cycles
    let hours = (c.power_on_instant.elapsed().as_secs() / 3600) as u128;
    buf[128..144].copy_from_slice(&hours.to_le_bytes());
    let nerr = c.stat_num_err_log_entries as u128;
    buf[176..192].copy_from_slice(&nerr.to_le_bytes());
    // **Phase Q8 + 12轮 L-Q8** — Crypto Erase generation 暴露到 SMART
    // vendor-specific 区域 (NVMe 2.0 § 5.16.1.2 offset 232..511 reserved/
    // vendor-specific)。driver 或 OEM 工具读 SMART[232..236] 拿 erase
    // generation 计数感知 Format SES=2 发生。
    buf[232..236].copy_from_slice(&c.crypto_gen.to_le_bytes());
    buf.truncate(bytes);
    buf
}

/// **Log Page 0x03** — Firmware Slot Information (spec § 5.16.1.3, 512 字节)。
///
/// Phase H5 真序列化 self.fw_active_slot / fw_next_active_slot /
/// fw_slot_revisions。AFI = (next << 4) | active；FRS[slot 1..7] @
/// offset 8 + (slot-1) * 8。
pub(super) fn build_fw_slot_info(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    let afi = ((c.fw_next_active_slot & 0x7) << 4) | (c.fw_active_slot & 0x7);
    buf[0] = afi;
    for slot in 1..=7usize {
        let rev = &c.fw_slot_revisions[slot];
        if rev.is_empty() {
            continue;
        }
        let off = (slot - 1) * 8 + 8;
        let rev_bytes = rev.as_bytes();
        let n = rev_bytes.len().min(8);
        buf[off..off + n].copy_from_slice(&rev_bytes[..n]);
        for b in buf[off + n..off + 8].iter_mut() {
            *b = b' ';
        }
    }
    buf.truncate(bytes);
    buf
}

/// **Log Page 0x06** — Device Self-Test (spec § 5.16.1.6, 564 字节)。
///
/// - offset 0: Current Self-Test Op (bits 3:0 STC)
/// - offset 1: Current Self-Test Completion (bits 6:0 percent)
/// - offset 4..32: Self-Test Result Data Structure[0] (最近一次完成)
///   - byte 0: STC nibble + Result nibble
///   - byte 4..12: Power On Hours 完成时刻快照
pub(super) fn build_self_test(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(564)];
    if let Some(ip) = &c.self_test_in_progress {
        buf[0] = ip.stc & 0x0f;
        buf[1] = ip.percent_complete & 0x7f;
    }
    if let Some(last) = c.self_test_last {
        buf[4] = ((last.stc & 0x0f) << 4) | (last.result & 0x0f);
        buf[8..16].copy_from_slice(&last.completed_at_poh.to_le_bytes());
    }
    buf.truncate(bytes);
    buf
}

/// **Phase H6 + S6** — Log Page 0x80 Reservation Notification (spec § 5.16.1.20)。
///
/// 64-byte entry：
///   bytes 0..8   log_page_count (u64 LE) — 该 ctrl 全局递增计数（重启不归 0）
///   bytes 8..9   log_page_type — 0 Empty / 1 Preempted / 2 Released /
///                              3 Registration Preempted
///   bytes 9..10  num_available — 同一 instant 还有几条未读
///   bytes 10..12 reserved
///   bytes 12..16 nsid
///   bytes 16..64 reserved
///
/// 返回当前 ring buffer 中所有 entries（最末 32 条；NVMe § 5.16.1.20 允许
/// controller-defined retention，我们用 ring 而非 read-then-clear）。
/// Driver 用 log_page_count 单调判定 stale，再次拉 log 会拿到同样 entries
/// 直到被新事件挤出 ring。
pub(super) fn build_reservation_notification(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let n_entries = c.reservation_notification_log.len();
    let need = (n_entries * 64).max(64);
    let mut buf = vec![0u8; bytes.max(need)];
    for (i, e) in c.reservation_notification_log.iter().enumerate() {
        let off = i * 64;
        if off + 16 > buf.len() {
            break;
        }
        buf[off..off + 8].copy_from_slice(&e.log_page_count.to_le_bytes());
        buf[off + 8] = e.log_page_type;
        // num_available = 剩余（自本 entry 起）
        buf[off + 9] = (n_entries - i - 1).min(255) as u8;
        buf[off + 12..off + 16].copy_from_slice(&e.nsid.to_le_bytes());
    }
    buf.truncate(bytes);
    buf
}

/// **Phase L2** — Log Page 0x09 Endurance Group Information (spec § 5.16.1.9)。
///
/// 512 字节，per endurance group。教学 controller 1 个 endurance group (id=1)，
/// 关键字段（spec § 5.16.1.7 Figure 211）：
///   byte 0..2 Critical Warning (bit 0=Spare<thresh / 1=Temp / 2=Reliability)
///   byte 2 Available Spare (% remaining)
///   byte 3 Available Spare Threshold
///   byte 4 Percentage Used (0-100)
///   bytes 32..48 Endurance Estimate (LBAs written 单位 1k)
///   bytes 48..64 Data Units Read (1k 单位)
///   bytes 64..80 Data Units Written (1k 单位)
///   bytes 80..96 Media and Data Integrity Errors
///   bytes 96..112 Number of Error Information Log Entries
pub(super) fn build_endurance_group(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    // critical_warning 0..2 = 0 (no warnings)
    buf[2] = 100; // Available Spare = 100%
    buf[3] = 10; // Available Spare Threshold
    buf[4] = 0; // Percentage Used = 0
    // Endurance Estimate @ 32..48: 1 PB / 1000 = 10^12 LBA units of 1k
    let endurance_est: u128 = 1_000_000_000_000u128;
    buf[32..48].copy_from_slice(&endurance_est.to_le_bytes());
    // Data Units Read @ 48..64: 同 SMART data_units_read
    let dur = (c.stat_lba_read.div_ceil(1000)) as u128;
    buf[48..64].copy_from_slice(&dur.to_le_bytes());
    // Data Units Written @ 64..80
    let duw = (c.stat_lba_written.div_ceil(1000)) as u128;
    buf[64..80].copy_from_slice(&duw.to_le_bytes());
    // Media and Data Integrity Errors @ 80..96
    let mdie = c.stat_num_err_log_entries as u128;
    buf[80..96].copy_from_slice(&mdie.to_le_bytes());
    // Number of Error Information Log Entries @ 96..112
    let nelogs = c.stat_num_err_log_entries as u128;
    buf[96..112].copy_from_slice(&nelogs.to_le_bytes());
    buf.truncate(bytes);
    buf
}

/// **Phase L2** — Log Page 0x0A Predictable Latency Per NVM Set (spec § 5.16.1.10)。
pub(super) fn build_predictable_latency_nvmset(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    buf.truncate(bytes);
    buf
}

/// **Phase L2** — Log Page 0x0B Predictable Latency Event Aggregate (spec § 5.16.1.11)。
pub(super) fn build_predictable_latency_event(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(8)];
    buf.truncate(bytes);
    buf
}

/// **Phase L2** — Log Page 0x0C Asymmetric Namespace Access (spec § 5.16.1.13)。
///
/// 16 byte header + per-ANA group descriptor。我们 1 controller，1 ANA group
/// containing 所有 NS，state = Optimized (0x01)。
pub(super) fn build_ana_log(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut nsids: Vec<u32> = c.namespaces.keys().collect();
    nsids.sort();
    let n_nsid = nsids.len() as u32;
    let total = 16 + 32 + 4 * (n_nsid as usize);
    let mut buf = vec![0u8; bytes.max(total)];
    // Header
    // bytes 0..8 = change count (incrementing per state change)
    buf[0..8].copy_from_slice(&c.ana_change_count.to_le_bytes());
    // bytes 8..12 = number of ANA Group Descriptors = 1
    buf[8..12].copy_from_slice(&1u32.to_le_bytes());
    // ANA Group Descriptor @ offset 16
    let off = 16;
    // bytes 0..4 of descriptor = ANA Group ID = 1
    buf[off..off + 4].copy_from_slice(&1u32.to_le_bytes());
    // bytes 4..8 = NumNSID
    buf[off + 4..off + 8].copy_from_slice(&n_nsid.to_le_bytes());
    // bytes 8..16 = Change Count（per-group），与 header 相同
    buf[off + 8..off + 16].copy_from_slice(&c.ana_change_count.to_le_bytes());
    // byte 16 = ANA State（spec 取值：0x01 Optimized / 0x02 Non-Optimized
    //                                / 0x03 Inaccessible / 0x04 Persistent Loss）
    buf[off + 16] = c.ana_state;
    // bytes 17..32 reserved
    // NSID list @ offset 16+32
    for (i, n) in nsids.iter().enumerate() {
        let no = 16 + 32 + i * 4;
        if no + 4 > buf.len() {
            break;
        }
        buf[no..no + 4].copy_from_slice(&n.to_le_bytes());
    }
    buf.truncate(bytes);
    buf
}

/// **Phase L2** — Log Page 0x0F Endurance Group Event Aggregate (spec § 5.16.1.12)。
pub(super) fn build_endurance_group_event(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(8)];
    buf.truncate(bytes);
    buf
}

/// **Phase K7 + Q3** — Log Page 0x07 Telemetry Host-Initiated (spec § 5.16.1.10)。
///
/// 512 字节 header + per-area data blocks。Telemetry 是 controller 内部
/// 诊断快照（generation #, data area 1/2/3 offsets, controller-defined
/// 字节）。Q3 真填 header + Data Area 1 = 一个 512-byte 数据块（含 host
/// I/O counters snapshot），让 driver `nvme telemetry-log -d 1 …` 能拿到
/// 真实诊断数据。
///
/// 字段布局（spec Figure 219）:
///   byte 0       Log Identifier = 0x07
///   bytes 1..5   reserved
///   bytes 5..8   IEEE OUI Identifier
///   bytes 8..10  Telemetry Host-Initiated Data Area 1 Last Block (1-based, 0=no data)
///   bytes 10..12 Area 2 Last Block
///   bytes 12..14 Area 3 Last Block
///   bytes 14..382 reserved
///   bytes 382..384 Telemetry Controller-Initiated Data Available (=0 host-init)
///   bytes 384..388 Area 4 Last Block (NVMe 2.0)
///   byte 388     Telemetry Host-Initiated Generation Number
///   byte 389     Telemetry Controller-Initiated Generation Number
///   bytes 390..512 Reason Identifier (vendor-specific)
pub(super) fn build_telemetry_host(c: &NvmeController, bytes: usize) -> Vec<u8> {
    // Allocate header + 1 block of Data Area 1 = 1024 byte 总 (512 header + 512 data)
    let total = (bytes).max(1024);
    let mut buf = vec![0u8; total];
    buf[0] = 0x07; // Log Identifier
    // Data Area 1 Last Block = 1 (1-based; 1 block = bytes 512..1024)
    buf[8..10].copy_from_slice(&1u16.to_le_bytes());
    buf[388] = 1; // Generation Number = 1
    // Reason Identifier @ 390..512 — 简化：写 "TEACH-NVME" ASCII
    let tag = b"TEACH-NVME";
    let n = tag.len().min(512 - 390);
    buf[390..390 + n].copy_from_slice(&tag[..n]);
    // Data Area 1 block @ 512..1024 — 写 host I/O counters snapshot
    // (total = bytes.max(1024) 恒 ≥ 1024，所以 fill 后由 truncate(bytes)
    // 决定 driver 是否真拿到 — driver 请求 < 1024 时只看到 header)
    let mut off = 512;
    let reads = c.stat_host_reads.to_le_bytes();
    buf[off..off + 8].copy_from_slice(&reads);
    off += 8;
    let writes = c.stat_host_writes.to_le_bytes();
    buf[off..off + 8].copy_from_slice(&writes);
    off += 8;
    let lba_read = c.stat_lba_read.to_le_bytes();
    buf[off..off + 8].copy_from_slice(&lba_read);
    off += 8;
    let lba_written = c.stat_lba_written.to_le_bytes();
    buf[off..off + 8].copy_from_slice(&lba_written);
    // 剩余 block 字节留 0
    buf.truncate(bytes);
    buf
}

/// **Phase K7 + Q3** — Log Page 0x08 Telemetry Controller-Initiated (同 0x07
/// 布局，content 由 controller 自主生成而非 driver 触发)。
pub(super) fn build_telemetry_ctrl(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let total = (bytes).max(1024);
    let mut buf = vec![0u8; total];
    buf[0] = 0x08;
    // Controller-Initiated Data Area 1 Last Block = 1
    buf[8..10].copy_from_slice(&1u16.to_le_bytes());
    // bytes 382..384 = "Telemetry Controller-Initiated Data Available" bit 0
    buf[382] = 0x01;
    buf[389] = 1; // Controller-Initiated Generation Number
    let tag = b"TEACH-NVME-CTRL";
    let n = tag.len().min(512 - 390);
    buf[390..390 + n].copy_from_slice(&tag[..n]);
    // Data Area 1: AEN 状态 + error log entry 数
    let aen_n = (c.aen_pending.len() as u64).to_le_bytes();
    buf[512..520].copy_from_slice(&aen_n);
    let err_n = c.stat_num_err_log_entries.to_le_bytes();
    buf[520..528].copy_from_slice(&err_n);
    buf.truncate(bytes);
    buf
}

/// **Phase K7** — Log Page 0x0D Persistent Event Log (spec § 5.16.1.14)。
///
/// 512 byte header + variable-length event records。Header：
/// - byte 0 = Log Identifier = 0x0D
/// - byte 1..4 reserved
/// - byte 4..8 = Total Number of Events (TNEV)
/// - byte 8..16 = Total Log Length (TLL)
/// - byte 16 = Log Revision (= 1 in spec)
/// - byte 17 reserved
/// - byte 18..20 = Log Header Length
///
/// 教学版：TNEV=0, TLL=512, 无 event 数据。
pub(super) fn build_persistent_event(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    buf[0] = 0x0D;
    // TLL @ 8..16 = 512
    buf[8..16].copy_from_slice(&512u64.to_le_bytes());
    buf[16] = 1; // Log Revision
    // Log Header Length @ 18..20 = 512
    buf[18..20].copy_from_slice(&512u16.to_le_bytes());
    buf.truncate(bytes);
    buf
}

/// **Phase K7** — Log Page 0x0E LBA Status Information (spec § 5.16.1.18)。
///
/// 报告 LBA 范围的 'unrecovered / pending media error' 状态。我们 backing
/// 无 ECC → 全无 error；header 设 'no descriptors'：
/// - byte 0..4 = Number of LBA Status Descriptors (NLSD) = 0
/// - byte 4..8 = Completion Condition Indicator (CCI) = 0 (no condition)
/// - byte 8..40 reserved
pub(super) fn build_lba_status_info(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(40)];
    buf.truncate(bytes);
    buf
}

/// **Phase K7** — Log Page 0x05 Commands Supported and Effects (spec § 5.16.1.5)。
///
/// 4096 字节：256 × 4 byte for admin opcodes (offset 0..1024) + 256 × 4
/// byte for NVM opcodes (offset 1024..2048) + rsvd。每 entry：
///   bit 0 = Command Supported (CSUPP)
///   bit 1 = Logical Block Content Change (LBCC) — 写盘类
///   bit 2 = Namespace Capability Change (NCC) — 改 NS 配置类
///   bit 3 = Namespace Inventory Change (NIC) — 创/删 NS
///   bit 4 = Controller Capability Change (CCC) — 改 controller 配置
///   bits 19:16 = Command Submission and Execution (CSE):
///     0 = no special, 1 = serialize per NS, 2 = serialize entire ctrl
pub(super) fn build_cmds_supported_effects(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(4096)];
    // CSE 字段 (bits 18:16)：
    //   0 = no special, 1 = serialize per NS, 2 = serialize controller-wide
    // **reviewer H5 修复**：Format / Sanitize / NS Mgmt 内部都拒绝
    // in-flight IO 期间执行（admin.rs in-flight 检查 + IO 路径 sanitize
    // 检查），等价"serialize controller-wide"，必须把 CSE=2 告诉 driver
    // 让其预先 quiesce 而非等 SC=0x84/0x12 反复重试。
    const CSE_CTRL_SERIALIZE: u32 = 0x2 << 16;
    // Helper: 写 entry
    let set = |buf: &mut [u8], opc: u8, base: usize, csupp: bool, flags: u32| {
        let off = base + (opc as usize) * 4;
        let v = if csupp { 0x1 | flags } else { 0 };
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    // Admin opcodes (base 0)
    set(&mut buf, 0x00, 0, true, 0); // Delete IO SQ — CCC
    set(&mut buf, 0x01, 0, true, 0x10);
    set(&mut buf, 0x02, 0, true, 0); // Get Log Page
    set(&mut buf, 0x04, 0, true, 0); // Delete IO CQ
    set(&mut buf, 0x05, 0, true, 0x10);
    set(&mut buf, 0x06, 0, true, 0); // Identify
    set(&mut buf, 0x08, 0, true, 0); // Abort
    set(&mut buf, 0x09, 0, true, 0x10); // Set Features — CCC
    set(&mut buf, 0x0a, 0, true, 0);
    set(&mut buf, 0x0c, 0, true, 0); // AER
    set(&mut buf, 0x0d, 0, true, 0x08 | 0x10 | CSE_CTRL_SERIALIZE); // NS Mgmt
    set(&mut buf, 0x10, 0, true, 0x10); // FW Commit
    set(&mut buf, 0x11, 0, true, 0); // FW Download
    set(&mut buf, 0x14, 0, true, 0); // Self-Test
    set(&mut buf, 0x15, 0, true, 0); // NS Attach
    set(&mut buf, 0x18, 0, true, 0); // Keep Alive
    set(&mut buf, 0x7c, 0, true, 0); // Doorbell Buffer Config
    set(&mut buf, 0x80, 0, true, 0x02 | 0x10 | CSE_CTRL_SERIALIZE); // Format
    set(&mut buf, 0x84, 0, true, 0x02 | 0x10 | CSE_CTRL_SERIALIZE); // Sanitize
    // NVM opcodes (base 1024)
    set(&mut buf, 0x00, 1024, true, 0); // Flush
    set(&mut buf, 0x01, 1024, true, 0x02); // Write
    set(&mut buf, 0x02, 1024, true, 0); // Read
    set(&mut buf, 0x05, 1024, true, 0); // Compare
    set(&mut buf, 0x08, 1024, true, 0x02); // Write Zeroes
    set(&mut buf, 0x09, 1024, true, 0x02); // DSM
    set(&mut buf, 0x0c, 1024, true, 0); // Verify
    set(&mut buf, 0x0d, 1024, true, 0); // Reservation Register
    set(&mut buf, 0x0e, 1024, true, 0); // Reservation Report
    set(&mut buf, 0x11, 1024, true, 0); // Reservation Acquire
    set(&mut buf, 0x15, 1024, true, 0); // Reservation Release
    buf.truncate(bytes);
    buf
}

/// **Phase K5** — Log Page 0x81 Sanitize Status (spec § 5.16.1.18, 512 byte)。
///
/// - offset 0..2 SPROG (sanitize progress) — 0..=65535 (=100% 时 0xFFFF)
/// - offset 2..4 SSTAT (sanitize status):
///   bits 2:0 = status (0=never, 1=success, 2=in-progress, 3=failed)
///   bits 7:3 = sanitize action causing 最近一次
///   bit 8    = global data erased
/// - offset 4..8 SCDW10 — driver 发起 Sanitize 时的 cdw10 副本
/// - offset 8..16 ETFO / ETFBE / ETFCE / ETFOW (estimated time)
pub(super) fn build_sanitize_status(c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(512)];
    // SPROG @ 0..2
    let sprog = c.sanitize.as_ref().map(|s| s.percent_complete).unwrap_or(0);
    buf[0..2].copy_from_slice(&sprog.to_le_bytes());
    // SSTAT @ 2..4，bits 2:0 = sanitize status
    let sstat = (c.sanitize_last_status as u16) & 0x7;
    buf[2..4].copy_from_slice(&sstat.to_le_bytes());
    buf.truncate(bytes);
    buf
}
