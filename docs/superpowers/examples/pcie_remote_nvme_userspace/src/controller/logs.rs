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

/// **Log Page 0x80** — Reservation Notification (spec § 5.16.1.15)。
///
/// 64 字节固定占位；ONCS.reservations 在 Phase H6 后 = 1，driver 可能真
/// 拉这个 log。结构详见 spec：8 byte log_page_count + 1 byte
/// log_page_type + 1 byte available_count + 2 reserved + 4 byte nsid +
/// 48 reserved。我们目前不真追踪通知队列，全 0 = "no notifications pending"。
pub(super) fn build_reservation_notification(_c: &NvmeController, bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes.max(64)];
    buf.truncate(bytes);
    buf
}
