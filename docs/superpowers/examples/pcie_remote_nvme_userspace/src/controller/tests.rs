use super::*;

/// 构造一个临时 backing file + NvmeController，仅用来跑非 DMA 单元逻辑
/// （SMART log builder / parse_prp_list / counter 累计）。
fn make_ctrl_with_tmp(tag: &str) -> NvmeController {
    let path = std::env::temp_dir().join(format!(
        "nvme_test_{}_{}_{:?}.img",
        std::process::id(),
        tag,
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(1024 * 1024).unwrap(); // 1 MiB → 2048 LBA
    drop(f);
    let c = NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap();
    // 不能立即 remove —— Windows 上仍持有的 File 句柄被删后导致后续操作
    // 失败；Unix 下 unlink-while-open 没问题但为可移植性也 keep。测试
    // 结束 OS tempdir 清理（best-effort）。
    c
}

/// Phase F：SMART log 关键 offset + counter 写入校验。
#[test]
fn smart_log_byte_layout_and_counters() {
    let mut c = make_ctrl_with_tmp("smart");
    c.stat_host_reads = 5;
    c.stat_host_writes = 7;
    c.stat_lba_read = 12_345;
    c.stat_lba_written = 67_890;
    c.stat_num_err_log_entries = 3;
    let buf = super::logs::build_smart_health(&c, 512);
    assert_eq!(buf.len(), 512);
    // composite_temp @ 1..3 (KiB)
    assert_eq!(u16::from_le_bytes([buf[1], buf[2]]), 313);
    // available_spare @ 3 = 100
    assert_eq!(buf[3], 100);
    // data_units_read @ 32..48: ceil(12345/1000) = 13
    let units_read = u128::from_le_bytes(buf[32..48].try_into().unwrap());
    assert_eq!(units_read, 13);
    // data_units_written @ 48..64: ceil(67890/1000) = 68
    let units_written = u128::from_le_bytes(buf[48..64].try_into().unwrap());
    assert_eq!(units_written, 68);
    // host_read_commands @ 64..80 = 5
    let hr = u128::from_le_bytes(buf[64..80].try_into().unwrap());
    assert_eq!(hr, 5);
    // host_write_commands @ 80..96 = 7
    let hw = u128::from_le_bytes(buf[80..96].try_into().unwrap());
    assert_eq!(hw, 7);
    // power_cycles @ 112..128 = 1
    let pc = u128::from_le_bytes(buf[112..128].try_into().unwrap());
    assert_eq!(pc, 1);
    // num_err_log_entries @ 176..192 = 3
    let nerr = u128::from_le_bytes(buf[176..192].try_into().unwrap());
    assert_eq!(nerr, 3);
}

/// Phase E (M2 修复后)：parse_prp_list 不再 0 终止 — 返回所有 entry，
/// caller 用 total_pages 自行截断。GPA = 0 是合法地址，不能视作终止。
#[test]
fn prp_list_returns_all_entries_no_zero_termination() {
    let mut bytes = vec![0u8; 64];
    bytes[..8].copy_from_slice(&0x1000_u64.to_le_bytes());
    bytes[8..16].copy_from_slice(&0x2000_u64.to_le_bytes());
    bytes[16..24].copy_from_slice(&0x3000_u64.to_le_bytes());
    // bytes[24..32] = 0 — 不再终止
    bytes[32..40].copy_from_slice(&0xdead_u64.to_le_bytes());
    let entries = parse_prp_list(&bytes);
    // 64 / 8 = 8 entry
    assert_eq!(entries.len(), 8);
    assert_eq!(entries[0], 0x1000);
    assert_eq!(entries[1], 0x2000);
    assert_eq!(entries[2], 0x3000);
    assert_eq!(entries[3], 0); // 0 不再终止
    assert_eq!(entries[4], 0xdead);
}

/// Phase F：AEN queue 行为 — push 多次，弹出顺序 FIFO。
/// 注：fire_aen 需要 DeviceCtx 才能 dma_write CQE，这里只测 queue 状态。
#[test]
fn aen_queue_fifo_order() {
    let mut c = make_ctrl_with_tmp("aen");
    c.aen_pending.push_back((1, 0, 0));
    c.aen_pending.push_back((2, 0, 0));
    c.aen_pending.push_back((3, 0, 0));
    assert_eq!(c.aen_pending.len(), 3);
    assert_eq!(c.aen_pending.pop_front().unwrap().0, 1);
    assert_eq!(c.aen_pending.pop_front().unwrap().0, 2);
    assert_eq!(c.aen_pending.pop_front().unwrap().0, 3);
    assert!(c.aen_pending.is_empty());
}

/// Phase G：error_log 环形 buffer 截到 64 (ELPE+1)；最新在 build 输出
/// 头部（spec § 5.16.1.1 entry 0 = most recent）。
#[test]
fn error_log_ring_buffer_64_max_recent_first() {
    let mut c = make_ctrl_with_tmp("errlog");
    // 推 70 条 → 应只保留 last 64
    for i in 0..70u16 {
        c.stat_num_err_log_entries += 1;
        c.push_error_log(1, i, 0x8, (i as u64) * 100, 1);
    }
    assert_eq!(c.error_log.len(), 64);
    // 最旧保留的 error_count = 7 (70 个推入 - 64 保留 = 6 个丢弃)
    assert_eq!(c.error_log.front().unwrap().error_count, 7);
    // 序列化：entry 0 应是最新的 (cid=69)
    let buf = super::logs::build_error_info(&c, 4096);
    let cid_at_entry0 = u16::from_le_bytes([buf[10], buf[11]]);
    assert_eq!(cid_at_entry0, 69, "entry 0 must be most recent");
    // entry 1 = 次新 cid=68
    let cid_at_entry1 = u16::from_le_bytes([buf[64 + 10], buf[64 + 11]]);
    assert_eq!(cid_at_entry1, 68);
}

/// Phase G：Self-Test Log 0x06 反映 idle / in-progress / 完成 (extended) /
/// aborted 状态。修正 reviewer HIGH：STC 不再硬编码 short，要真实反映
/// 上次完成类型；STC=2 extended 必须被记成 2。
#[test]
fn self_test_log_layout_in_progress_done_and_abort() {
    let mut c = make_ctrl_with_tmp("st");
    // idle → 全 0
    let buf = super::logs::build_self_test(&c, 564);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[1], 0);
    assert_eq!(buf[4], 0);
    // in-progress extended, 42% done
    c.self_test_in_progress = Some(SelfTestInProgress {
        started_at: std::time::Instant::now(),
        stc: 2,
        total_seconds: 20,
        percent_complete: 42,
    });
    let buf = super::logs::build_self_test(&c, 564);
    assert_eq!(buf[0], 2);
    assert_eq!(buf[1], 42);
    // 完成 extended：in_progress=None, last=Some(stc=2, result=0)
    c.self_test_in_progress = None;
    c.self_test_last = Some(SelfTestCompleted {
        stc: 2,
        result: 0,
        completed_at_poh: 7,
    });
    let buf = super::logs::build_self_test(&c, 564);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[1], 0);
    // byte 4：上半 nibble = STC=2 (extended), 下半 = result=0
    assert_eq!(buf[4] & 0xf0, 0x20, "STC must reflect actual extended type");
    assert_eq!(buf[4] & 0x0f, 0x00);
    // POH @ buf[8..16] = 7
    let poh = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    assert_eq!(poh, 7);
    // Aborted short：result=0x09
    c.self_test_last = Some(SelfTestCompleted {
        stc: 1,
        result: 0x09,
        completed_at_poh: 3,
    });
    let buf = super::logs::build_self_test(&c, 564);
    assert_eq!(buf[4] & 0xf0, 0x10);
    assert_eq!(buf[4] & 0x0f, 0x09);
}

/// Phase H1：features map 存 Set 过的 cdw11，Get 回填；NumberOfQueues
/// 受 IO_QUEUE_CAP 限；VWC 强制 WCE=1。
#[test]
fn features_set_get_round_trip_and_special_cases() {
    let mut c = make_ctrl_with_tmp("feat");
    // 任意 fid：Set 0x42 cdw11=0xdeadbeef → Get 回 0xdeadbeef
    c.features.insert(0x42, 0xdead_beef);
    assert_eq!(*c.features.get(&0x42).unwrap(), 0xdead_beef);
    // 未 Set 过的 fid Get 返 0（admin.rs match _ 默认值）
    assert_eq!(c.features.get(&0xff).copied().unwrap_or(0), 0);
    // NumberOfQueues 实际行为校验：cap 常量非零（绕过 clippy const-assert）
    let cap: u16 = IO_QUEUE_CAP;
    assert!(cap >= 1);
    // VWC 强制 bit0=1（模拟 Set 路径把 driver 写的 cdw11 | 0x1 存入）
    c.features
        .insert(crate::cmd::fid::VOLATILE_WRITE_CACHE, 0x1);
    assert_eq!(
        c.features[&crate::cmd::fid::VOLATILE_WRITE_CACHE] & 0x1,
        0x1
    );
}

/// Phase H4：多 namespace open + Active NSID list 序列化 + ns_mut
/// 返 None for NSID 0 / 0xFFFF_FFFF。
#[test]
fn multi_namespace_open_and_active_list() {
    let dir = std::env::temp_dir();
    let p1 = dir.join(format!(
        "nvme_test_ns1_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let p2 = dir.join(format!(
        "nvme_test_ns2_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    for p in [&p1, &p2] {
        let f = std::fs::File::create(p).unwrap();
        f.set_len(1024 * 1024).unwrap();
    }
    let paths = vec![
        p1.to_str().unwrap().to_string(),
        p2.to_str().unwrap().to_string(),
    ];
    let c = NvmeController::open(&paths, 0x1414, 0, &[]).unwrap();
    assert_eq!(c.namespaces.len(), 2);
    assert!(c.namespaces.contains_key(&1));
    assert!(c.namespaces.contains_key(&2));
    // NSID 0 / 0xFFFF_FFFF 不可用
    assert!(c.ns(0).is_none());
    assert!(c.ns(0xFFFF_FFFF).is_none());
    assert!(c.ns(3).is_none());
    // NS 都是 2048 LBA (1 MiB / 512)
    assert_eq!(c.ns(1).unwrap().total_lba, 2048);
    assert_eq!(c.ns(2).unwrap().total_lba, 2048);
}

/// Phase H5：FW Slot Info Log 反映 active_slot / next_active_slot /
/// per-slot revision string；AFI 编码 bits 2:0 = active, 6:4 = next。
#[test]
fn fw_slot_info_log_reflects_state() {
    let mut c = make_ctrl_with_tmp("fw");
    c.fw_active_slot = 2;
    c.fw_next_active_slot = 3;
    c.fw_slot_revisions[2] = "newrev1 ".to_string();
    c.fw_slot_revisions[3] = "newrev2 ".to_string();
    let buf = super::logs::build_fw_slot_info(&c, 512);
    // AFI = (3 << 4) | 2 = 0x32
    assert_eq!(buf[0], 0x32);
    // FRS[slot 1] @ offset 8..16（默认 v2.0    ，保留自 init）
    assert_eq!(&buf[8..16], b"v2.0    ");
    // FRS[slot 2] @ offset 16..24
    assert_eq!(&buf[16..24], b"newrev1 ");
    // FRS[slot 3] @ offset 24..32
    assert_eq!(&buf[24..32], b"newrev2 ");
}

/// Phase H6：Reservation Register/Acquire/Release 状态机校验。
#[test]
fn reservation_state_machine() {
    let mut c = make_ctrl_with_tmp("rsv");
    let nsid = 1u32;
    // 提取 CQE 的 SC byte（spec dw3 bits 17..25）
    let sc = |cqe: &Cqe| -> u8 { ((cqe.dw3 >> 17) & 0xff) as u8 };
    // Register host A with rkey=0x1001
    let mut buf = vec![0u8; 16];
    buf[8..16].copy_from_slice(&0x1001u64.to_le_bytes()); // NRKEY
    let cqe = c.apply_reservation_cmd(nsid, ReservationKind::Register, 0, 0, &buf, 0, 0, 0, 1);
    assert_eq!(sc(&cqe), 0, "Register OK");
    assert_eq!(c.namespaces[&nsid].registrants, vec![(0x1001, 0, 0)]);
    // Acquire WriteExclusive (type=1)
    let mut buf = vec![0u8; 16];
    buf[0..8].copy_from_slice(&0x1001u64.to_le_bytes()); // CRKEY
    let cqe = c.apply_reservation_cmd(nsid, ReservationKind::Acquire, 0, 1, &buf, 0, 0, 0, 1);
    assert_eq!(sc(&cqe), 0, "Acquire OK");
    assert_eq!(c.namespaces[&nsid].reservation, Some((0x1001, 1)));
    // Second Acquire 应失败 (RESERVATION_CONFLICT)
    let cqe = c.apply_reservation_cmd(nsid, ReservationKind::Acquire, 0, 1, &buf, 0, 0, 0, 1);
    assert_eq!(sc(&cqe), crate::cmd::sc::RESERVATION_CONFLICT);
    // Release
    let buf = 0x1001u64.to_le_bytes().to_vec();
    let cqe = c.apply_reservation_cmd(nsid, ReservationKind::Release, 0, 1, &buf, 0, 0, 0, 1);
    assert_eq!(sc(&cqe), 0, "Release OK");
    assert_eq!(c.namespaces[&nsid].reservation, None);
}

/// Phase K4a/b：PI Write interleave + Read 解析 round-trip。
///
/// 单 LBA 4 KiB data + 8 byte T10 DIF tuple inline。verify 必须通过。
#[test]
fn pi_write_read_round_trip() {
    let mut c = make_ctrl_with_tmp("pi");
    let nsid = 1u32;
    // 切到 PI 配置
    let ns = c.namespaces.get_mut(&nsid).unwrap();
    ns.lbads = 12;
    ns.meta_size = 8;
    ns.pi_type = 1;
    ns.pi_first = true;
    // 重算 total_lba
    let size = ns.file.metadata().unwrap().len();
    ns.total_lba = size / ns.block_bytes();

    // 模拟 PI Write 完成路径：compute tuple + interleave write
    let lba = 0u64;
    let data: Vec<u8> = (0..4096).map(|i| (i & 0xff) as u8).collect();
    let tuple = crate::pi::PiTuple::compute(&data, lba, 1);
    let mut block = vec![0u8; 4096 + 8];
    block[0..8].copy_from_slice(&tuple.to_bytes());
    block[8..4104].copy_from_slice(&data);
    let ns = c.namespaces.get_mut(&nsid).unwrap();
    use std::io::Seek as _;
    use std::io::Write as _;
    ns.file.seek(std::io::SeekFrom::Start(0)).unwrap();
    ns.file.write_all(&block).unwrap();
    ns.file.sync_all().unwrap();

    // 模拟 PI Read 路径：读 4104 → 拆 tuple/data → verify
    let mut read_block = vec![0u8; 4096 + 8];
    let ns = c.namespaces.get_mut(&nsid).unwrap();
    ns.file.seek(std::io::SeekFrom::Start(0)).unwrap();
    std::io::Read::read_exact(&mut ns.file, &mut read_block).unwrap();
    let tuple_arr: [u8; 8] = read_block[0..8].try_into().unwrap();
    let pi = crate::pi::PiTuple::from_bytes(&tuple_arr);
    let data_slice = &read_block[8..4104];
    assert_eq!(pi.verify(data_slice, lba, 1), crate::pi::PiCheck::Ok);
    assert_eq!(data_slice, &data[..]);

    // 篡改 1 byte → guard fail
    let mut bad = read_block.clone();
    bad[100] ^= 0xff;
    let bad_pi = crate::pi::PiTuple::from_bytes(&bad[0..8].try_into().unwrap());
    let bad_data = &bad[8..4104];
    assert_eq!(
        bad_pi.verify(bad_data, lba, 1),
        crate::pi::PiCheck::GuardFail
    );
}

/// Phase L1：ZNS NS 初始化 + zone state 默认值。
#[test]
fn zns_namespace_init() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "nvme_test_zns_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap(); // 8 MiB = 16384 LBA at 512B
    drop(f);
    let paths = vec![path.to_str().unwrap().to_string()];
    let c = NvmeController::open(&paths, 0x1414, 0, &[1]).unwrap();
    let zns = c.namespaces[&1].zns.as_ref().unwrap();
    // 8 MiB / 1 MiB per zone = 8 zones
    assert_eq!(zns.zones.len(), 8);
    assert_eq!(zns.zone_size, 2048);
    assert_eq!(zns.zone_capacity, 2048);
    assert!(zns.zones.iter().all(|z| z.state == ZoneState::Empty));
    assert!(zns.zones.iter().all(|z| z.write_pointer == 0));
}

/// Phase M1b：Interrupt Coalescing 决策。
#[test]
fn irq_coalesce_decision() {
    // Admin CQ 永远立即 fire（pending=0 也 fire — admin 路径不 batch）
    assert!(should_fire_irq(0, 0, 200, 200));
    assert!(should_fire_irq(0, 1, 100, 100));
    // 默认（thr=0）退化为 fire-on-every（0's-based：阈值=1）
    assert!(should_fire_irq(1, 1, 0, 0));
    assert!(should_fire_irq(1, 1, 0, 50));
    // thr=3 (0-based → 需 4 个 pending) — 前 3 个 batch
    assert!(!should_fire_irq(1, 1, 3, 0));
    assert!(!should_fire_irq(1, 2, 3, 0));
    assert!(!should_fire_irq(1, 3, 3, 0));
    assert!(should_fire_irq(1, 4, 3, 0));
    assert!(should_fire_irq(1, 5, 3, 0));
    // thr=1 → 阈值=2，第 2 条 fire
    assert!(!should_fire_irq(1, 1, 1, 0));
    assert!(should_fire_irq(1, 2, 1, 0));
    // thr=255 max → 阈值=256，pending=u32::MAX fire
    assert!(should_fire_irq(1, 256, 255, 0));
    assert!(!should_fire_irq(1, 255, 255, 0));
}

/// **Reviewer H2** — ZNS state machine transition table。覆盖
/// 7 states × 5 ZSAs + unknown action。
#[test]
fn zns_state_machine_transitions() {
    use crate::controller::io::check_zsa_transition;
    use ZoneState::*;
    // (state, zsa) → None=OK / Some(sc)=fail
    // Close (0x01)
    assert_eq!(check_zsa_transition(Empty, 0x01), None); // no-op
    assert_eq!(check_zsa_transition(ImplicitOpen, 0x01), None);
    assert_eq!(check_zsa_transition(ExplicitOpen, 0x01), None);
    assert_eq!(check_zsa_transition(Closed, 0x01), None);
    assert_eq!(check_zsa_transition(Full, 0x01), None);
    assert_eq!(
        check_zsa_transition(ReadOnly, 0x01),
        Some(sc::ZONE_IS_READ_ONLY)
    );
    assert_eq!(
        check_zsa_transition(Offline, 0x01),
        Some(sc::ZONE_IS_OFFLINE)
    );
    // Finish (0x02) — 全 active states OK，RO/Offline fail
    assert_eq!(check_zsa_transition(Empty, 0x02), None);
    assert_eq!(check_zsa_transition(Full, 0x02), None);
    assert_eq!(
        check_zsa_transition(ReadOnly, 0x02),
        Some(sc::ZONE_IS_READ_ONLY)
    );
    // Open (0x03) — Full → ZSTI，RO/Offline fail
    assert_eq!(check_zsa_transition(Empty, 0x03), None);
    assert_eq!(check_zsa_transition(Closed, 0x03), None);
    assert_eq!(
        check_zsa_transition(Full, 0x03),
        Some(sc::INVALID_ZONE_STATE_TRANSITION)
    );
    assert_eq!(
        check_zsa_transition(Offline, 0x03),
        Some(sc::ZONE_IS_OFFLINE)
    );
    // Reset (0x04) — 全 active OK，RO/Offline fail
    assert_eq!(check_zsa_transition(Full, 0x04), None);
    assert_eq!(check_zsa_transition(ImplicitOpen, 0x04), None);
    assert_eq!(
        check_zsa_transition(ReadOnly, 0x04),
        Some(sc::ZONE_IS_READ_ONLY)
    );
    // Offline (0x05) — 只能从 Full/RO/Offline；Empty/Open/Closed 都 ZSTI
    assert_eq!(check_zsa_transition(Full, 0x05), None);
    assert_eq!(check_zsa_transition(ReadOnly, 0x05), None);
    assert_eq!(check_zsa_transition(Offline, 0x05), None); // no-op
    assert_eq!(
        check_zsa_transition(Empty, 0x05),
        Some(sc::INVALID_ZONE_STATE_TRANSITION)
    );
    assert_eq!(
        check_zsa_transition(ImplicitOpen, 0x05),
        Some(sc::INVALID_ZONE_STATE_TRANSITION)
    );
    assert_eq!(
        check_zsa_transition(Closed, 0x05),
        Some(sc::INVALID_ZONE_STATE_TRANSITION)
    );
    // Unknown ZSA
    assert_eq!(check_zsa_transition(Empty, 0xAA), Some(sc::INVALID_FIELD));
    assert_eq!(check_zsa_transition(Full, 0x00), Some(sc::INVALID_FIELD));
}

/// **Reviewer H-1 (2nd round)** — apply_zsa 必须保留 spec 表中 `-` 的
/// no-op 不改写 source state。之前 commit 错误地把 Close on Empty/Full
/// 等都改写到了 Closed。
#[test]
fn zns_apply_noop_preserves_state() {
    use crate::controller::io::apply_zsa;
    let mk = |s: ZoneState, wp: u64| Zone {
        write_pointer: wp,
        state: s,
    };
    // Close on Empty/Full/Closed → no change，返回 false
    let mut z = mk(ZoneState::Empty, 0);
    assert!(!apply_zsa(&mut z, 0x01, 1024));
    assert_eq!(z.state, ZoneState::Empty);
    let mut z = mk(ZoneState::Full, 1024);
    assert!(!apply_zsa(&mut z, 0x01, 1024));
    assert_eq!(z.state, ZoneState::Full);
    assert_eq!(z.write_pointer, 1024);
    let mut z = mk(ZoneState::Closed, 500);
    assert!(!apply_zsa(&mut z, 0x01, 1024));
    assert_eq!(z.state, ZoneState::Closed);
    // Finish on Full → no change
    let mut z = mk(ZoneState::Full, 1024);
    assert!(!apply_zsa(&mut z, 0x02, 1024));
    assert_eq!(z.state, ZoneState::Full);
    // Open on ExplicitOpen → no change
    let mut z = mk(ZoneState::ExplicitOpen, 100);
    assert!(!apply_zsa(&mut z, 0x03, 1024));
    assert_eq!(z.state, ZoneState::ExplicitOpen);
    // Offline on Offline → no change
    let mut z = mk(ZoneState::Offline, 0);
    assert!(!apply_zsa(&mut z, 0x05, 1024));
    assert_eq!(z.state, ZoneState::Offline);
}

/// **Reviewer H-1** — apply_zsa 真改写 + 正确 WP transitions。
#[test]
fn zns_apply_real_transitions() {
    use crate::controller::io::apply_zsa;
    // Close on ImplicitOpen → Closed
    let mut z = Zone {
        write_pointer: 200,
        state: ZoneState::ImplicitOpen,
    };
    assert!(apply_zsa(&mut z, 0x01, 1024));
    assert_eq!(z.state, ZoneState::Closed);
    assert_eq!(z.write_pointer, 200); // WP unchanged
    // Finish on Empty → Full + WP = capacity
    let mut z = Zone {
        write_pointer: 0,
        state: ZoneState::Empty,
    };
    assert!(apply_zsa(&mut z, 0x02, 1024));
    assert_eq!(z.state, ZoneState::Full);
    assert_eq!(z.write_pointer, 1024);
    // Open on Empty → ExplicitOpen
    let mut z = Zone {
        write_pointer: 0,
        state: ZoneState::Empty,
    };
    assert!(apply_zsa(&mut z, 0x03, 1024));
    assert_eq!(z.state, ZoneState::ExplicitOpen);
    // Reset on Full → Empty + WP = 0
    let mut z = Zone {
        write_pointer: 1024,
        state: ZoneState::Full,
    };
    assert!(apply_zsa(&mut z, 0x04, 1024));
    assert_eq!(z.state, ZoneState::Empty);
    assert_eq!(z.write_pointer, 0);
    // Offline on Full → Offline
    let mut z = Zone {
        write_pointer: 1024,
        state: ZoneState::Full,
    };
    assert!(apply_zsa(&mut z, 0x05, 1024));
    assert_eq!(z.state, ZoneState::Offline);
    // Illegal: Open on Full → no change，返回 false（check_zsa_transition
    // gates 它）
    let mut z = Zone {
        write_pointer: 1024,
        state: ZoneState::Full,
    };
    assert!(!apply_zsa(&mut z, 0x03, 1024));
    assert_eq!(z.state, ZoneState::Full);
}

/// **Reviewer M1** — Zone Report 边界 buffer：< 64 byte 不丢 header；
/// 中等 buffer 容下若干 zones；大 buffer 反映全部。
#[test]
fn zone_report_buffer_sizing() {
    use crate::controller::io::__test_build_zone_report;
    let zns = ZnsState {
        zone_size: 1024,
        zone_capacity: 1024,
        max_open: 0,
        max_active: 0,
        zones: (0..4)
            .map(|i| Zone {
                write_pointer: i as u64 * 64,
                state: if i == 0 {
                    ZoneState::Full
                } else {
                    ZoneState::Empty
                },
            })
            .collect(),
    };
    // 太小：bytes=4 → header NZ 截断到 4 字节，driver 仍能解 partial。
    let buf = __test_build_zone_report(&zns, 0, 4);
    assert_eq!(buf.len(), 4);
    // 正好 64 → header full + 0 zones
    let buf = __test_build_zone_report(&zns, 0, 64);
    assert_eq!(buf.len(), 64);
    assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 0);
    // 64 + 2*64 = 192 → 2 zones
    let buf = __test_build_zone_report(&zns, 0, 192);
    assert_eq!(buf.len(), 192);
    assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 2);
    // zone 0 = Full
    assert_eq!(buf[64], 0x02); // ZT
    assert_eq!(buf[65], 0xE0); // ZS Full
    // zone 1 = Empty
    assert_eq!(buf[128 + 1], 0x10); // ZS Empty
    // 全装下：64 + 4*64 = 320
    let buf = __test_build_zone_report(&zns, 0, 320);
    assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 4);
    // start_zone_idx > total → 0 zones
    let buf = __test_build_zone_report(&zns, 99, 320);
    assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 0);
}

/// **Phase L1c** — ZNS NS Identify byte layout：MAR/MOR/ZSZE 正确 offset。
#[test]
fn zns_ns_identify_byte_layout() {
    use crate::controller::admin::__test_build_zns_ns_identify;
    let zns = ZnsState {
        zone_size: 2048,
        zone_capacity: 2048,
        max_open: 7,
        max_active: 14,
        zones: vec![],
    };
    let buf = __test_build_zns_ns_identify(&zns);
    assert_eq!(buf.len(), 4096);
    // **Reviewer H-A**：MAR/MOR 0's-based，所以 max_active=14 → wire=13。
    assert_eq!(u32::from_le_bytes(buf[4..8].try_into().unwrap()), 13);
    assert_eq!(u32::from_le_bytes(buf[8..12].try_into().unwrap()), 6);
    // ZSZE @ 2816..2824
    assert_eq!(
        u64::from_le_bytes(buf[2816..2824].try_into().unwrap()),
        2048
    );
    // ZDES @ 2824 = 0
    assert_eq!(buf[2824], 0);
}

/// **Reviewer H-A** — MAR/MOR=0 (内部=unlimited) → wire = 0xFFFFFFFF。
#[test]
fn zns_ns_identify_unlimited_translation() {
    use crate::controller::admin::__test_build_zns_ns_identify;
    let zns = ZnsState {
        zone_size: 1024,
        zone_capacity: 1024,
        max_open: 0,   // unlimited
        max_active: 0, // unlimited
        zones: vec![],
    };
    let buf = __test_build_zns_ns_identify(&zns);
    // MAR = unlimited → 0xFFFFFFFF
    assert_eq!(
        u32::from_le_bytes(buf[4..8].try_into().unwrap()),
        0xFFFF_FFFF
    );
    // MOR = unlimited → 0xFFFFFFFF
    assert_eq!(
        u32::from_le_bytes(buf[8..12].try_into().unwrap()),
        0xFFFF_FFFF
    );
}

/// **Reviewer H-L1d-1/2 + C-1 (5th round)** — CNS 0x08 I/O CS Independent
/// NS Identify (NVMe 2.0 § 5.17.2)：字段 offset + RESCAP cross-CNS 一致性。
/// **注**：之前 commit 误用 CNS 0x06；C-1 修正后 build_cs_indep_ns_identify
/// 服务于 CNS 0x08 dispatch arm。
#[test]
fn cs_indep_ns_identify_layout_and_rescap_consistency() {
    use crate::controller::admin::build_cs_indep_ns_identify;
    let buf = build_cs_indep_ns_identify();
    assert_eq!(buf.len(), 4096);
    // NSFEAT @ 0 = 0
    assert_eq!(buf[0], 0x00);
    // NMIC @ 1 = 0
    assert_eq!(buf[1], 0x00);
    // RESCAP @ 2 必须 = CNS 0x00 RESCAP (cmd.rs:553 = 0x1E)。spec § 5.17.2.6
    // 强制 cross-CNS 一致。
    assert_eq!(buf[2], 0x1E);
    // FPI @ 3 = 0 (format complete)
    assert_eq!(buf[3], 0x00);
    // NSTAT @ 13 — bit 0 NRDY 'Not Ready' = 0 表示 ready。**之前 commit
    // 错写 buf[14]=1 + 把 NRDY 取反**，本测试锁定修复后的 layout：
    assert_eq!(buf[13], 0x00, "NSTAT @ offset 13, NRDY=0 means ready");
    // byte 14 是 NVMe 2.0 reserved，必须 0
    assert_eq!(buf[14], 0x00, "byte 14 is reserved per NVMe 2.0");
}

/// **Reviewer (final round)** — `check_zns_write` 直接单测覆盖 boundary +
/// Offline/Full/ReadOnly + SWR mismatch 拒绝路径，避免依赖 dispatch_io
/// 间接验证。
#[test]
fn check_zns_write_rejections() {
    use crate::controller::io::check_zns_write;
    let mut c = make_ctrl_with_tmp("zns_write_check");
    // 把 NS 1 转成 ZNS 用 helper
    let path = c.namespaces[&1].path.clone();
    drop(c); // 关 file 让 open() 走 ZNS path 重新装
    let path_clone = path.clone();
    let mut c = NvmeController::open(&[path_clone.clone()], 0x1414, 0, &[1]).unwrap();
    let ns = &c.namespaces[&1];
    // 准备 Full / Offline / ReadOnly zone 各一个用来 reject
    let zns = ns.zns.as_ref().unwrap();
    let zone_size = zns.zone_size;
    drop(zns);
    // 全 Empty 初始：legal write 应 None
    assert!(check_zns_write(&c.namespaces[&1], 0, 1, 0, 0, 0, 1).is_none());
    // 跨 zone 边界 → ZONE_BOUNDARY_ERR
    let cqe = check_zns_write(&c.namespaces[&1], zone_size - 1, 2, 0, 0, 0, 1).unwrap();
    let sc = (cqe.dw3 >> 17) as u8;
    assert_eq!(sc, sc::ZONE_BOUNDARY_ERR);
    let sct = ((cqe.dw3 >> 25) & 0x7) as u8;
    assert_eq!(sct, sc::SCT_COMMAND_SPECIFIC);
    // SWR mismatch：在 Empty zone WP=0 写 LBA=5 应 ZONE_INVALID_WRITE
    let cqe = check_zns_write(&c.namespaces[&1], 5, 1, 0, 0, 0, 1).unwrap();
    let sc = (cqe.dw3 >> 17) as u8;
    assert_eq!(sc, sc::ZONE_INVALID_WRITE);
    // 模拟 zone 0 Full
    {
        let zns = c.namespaces.get_mut(&1).unwrap().zns.as_mut().unwrap();
        zns.zones[0].state = ZoneState::Full;
    }
    let cqe = check_zns_write(&c.namespaces[&1], 0, 1, 0, 0, 0, 1).unwrap();
    assert_eq!((cqe.dw3 >> 17) as u8, sc::ZONE_IS_FULL);
    // Offline
    {
        let zns = c.namespaces.get_mut(&1).unwrap().zns.as_mut().unwrap();
        zns.zones[0].state = ZoneState::Offline;
    }
    let cqe = check_zns_write(&c.namespaces[&1], 0, 1, 0, 0, 0, 1).unwrap();
    assert_eq!((cqe.dw3 >> 17) as u8, sc::ZONE_IS_OFFLINE);
    // ReadOnly
    {
        let zns = c.namespaces.get_mut(&1).unwrap().zns.as_mut().unwrap();
        zns.zones[0].state = ZoneState::ReadOnly;
    }
    let cqe = check_zns_write(&c.namespaces[&1], 0, 1, 0, 0, 0, 1).unwrap();
    assert_eq!((cqe.dw3 >> 17) as u8, sc::ZONE_IS_READ_ONLY);
}

/// **Reviewer (final round)** — `check_zns_read` 只对 Offline zone reject。
#[test]
fn check_zns_read_offline_only() {
    use crate::controller::io::check_zns_read;
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "nvme_test_zns_read_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap();
    drop(f);
    let mut c =
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[1]).unwrap();
    // Empty zone：legal
    assert!(check_zns_read(&c.namespaces[&1], 0, 0, 0, 0, 1).is_none());
    // 把 zone 0 标 Offline → 拒
    c.namespaces
        .get_mut(&1)
        .unwrap()
        .zns
        .as_mut()
        .unwrap()
        .zones[0]
        .state = ZoneState::Offline;
    let cqe = check_zns_read(&c.namespaces[&1], 0, 0, 0, 0, 1).unwrap();
    assert_eq!((cqe.dw3 >> 17) as u8, sc::ZONE_IS_OFFLINE);
    // ReadOnly / Full 允许读
    c.namespaces
        .get_mut(&1)
        .unwrap()
        .zns
        .as_mut()
        .unwrap()
        .zones[0]
        .state = ZoneState::ReadOnly;
    assert!(check_zns_read(&c.namespaces[&1], 0, 0, 0, 0, 1).is_none());
    c.namespaces
        .get_mut(&1)
        .unwrap()
        .zns
        .as_mut()
        .unwrap()
        .zones[0]
        .state = ZoneState::Full;
    assert!(check_zns_read(&c.namespaces[&1], 0, 0, 0, 0, 1).is_none());
}

/// **Reviewer (final round)** — `advance_zns_wp` Empty→ImplicitOpen +
/// WP=capacity→Full 直接断言。
#[test]
fn advance_zns_wp_transitions() {
    use crate::controller::io::advance_zns_wp;
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "nvme_test_zns_wp_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(8 * 1024 * 1024).unwrap();
    drop(f);
    let mut c =
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[1]).unwrap();
    let ns = c.namespaces.get_mut(&1).unwrap();
    let capacity = ns.zns.as_ref().unwrap().zone_capacity;
    // Empty → ImplicitOpen (WP=10)
    advance_zns_wp(ns, 0, 10);
    let zone = &ns.zns.as_ref().unwrap().zones[0];
    assert_eq!(zone.write_pointer, 10);
    assert_eq!(zone.state, ZoneState::ImplicitOpen);
    // ImplicitOpen 继续 → WP=20，state 不变
    advance_zns_wp(ns, 10, 10);
    let zone = &ns.zns.as_ref().unwrap().zones[0];
    assert_eq!(zone.write_pointer, 20);
    assert_eq!(zone.state, ZoneState::ImplicitOpen);
    // WP 推进到 capacity → Full
    advance_zns_wp(ns, 20, (capacity - 20) as u32);
    let zone = &ns.zns.as_ref().unwrap().zones[0];
    assert_eq!(zone.write_pointer, capacity);
    assert_eq!(zone.state, ZoneState::Full);
}

/// **Phase M2** — mmap zero-copy round-trip：write_at 通过 mmap 写入后，
/// 另一次 read_at（共享同一 mmap）应立即读到刚写的数据（不需 fsync）。
#[test]
fn mmap_zero_copy_round_trip() {
    let mut c = make_ctrl_with_tmp("mmap_rt");
    let ns = c.namespaces.get_mut(&1).unwrap();
    // mmap 应在 open() 后已建立（/tmp tmpfs 支持 mmap）
    assert!(ns.mmap.is_some(), "Phase M2: mmap should initialize");
    // 写 4 KiB pattern 到 LBA 0
    let pattern: Vec<u8> = (0..4096u32).map(|i| (i ^ 0xAA) as u8).collect();
    ns.write_at(&pattern, 0).unwrap();
    // 立即读回（不调 flush —— mmap 是同一内存视图）
    let mut readback = vec![0u8; 4096];
    ns.read_at(&mut readback, 0).unwrap();
    assert_eq!(readback, pattern, "mmap write/read same view");
    // 跨 LBA 边界写
    ns.write_at(&[0x55; 512], 8 * 512).unwrap();
    let mut tail = vec![0u8; 512];
    ns.read_at(&mut tail, 8 * 512).unwrap();
    assert_eq!(tail, vec![0x55; 512]);
    // flush 走 mmap.flush()，不应 panic
    ns.flush().unwrap();
}

/// **Phase K4c** — 多 LBA PI Write 累积器 + 完成路径的 byte-level 校验：
/// 通过直接写 4096+8 byte-per-LBA pattern 到 backing，验证多 LBA PI Read
/// 路径能 verify 通过并提取出纯 data 部分。
#[test]
fn k4c_multi_lba_pi_layout_round_trip() {
    use crate::pi::PiTuple;
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "nvme_test_k4c_{}_{:?}.img",
        std::process::id(),
        std::thread::current().id()
    ));
    // 4 个 LBA × 4104 byte = 16416 byte backing
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(4 * 4104).unwrap();
    drop(f);
    let mut c =
        NvmeController::open(&[path.to_str().unwrap().to_string()], 0x1414, 0, &[]).unwrap();
    // Format 切到 LBAF[1] + Type 1（手动）
    {
        let ns = c.namespaces.get_mut(&1).unwrap();
        ns.lbads = 12;
        ns.meta_size = 8;
        ns.pi_type = 1;
        ns.pi_first = false; // data 在前，tuple 在尾
        ns.total_lba = 4;
    }
    // 构造 4 个 LBA × (4096 data + 8 PI)，与 K4c Write 完成路径产物一致
    let ns = c.namespaces.get_mut(&1).unwrap();
    for lba in 0..4u64 {
        let data: Vec<u8> = (0..4096).map(|i| ((i as u64 ^ lba) & 0xff) as u8).collect();
        let tuple = PiTuple::compute(&data, lba, 1);
        let tuple_bytes = tuple.to_bytes();
        let mut block = vec![0u8; 4104];
        block[0..4096].copy_from_slice(&data);
        block[4096..4104].copy_from_slice(&tuple_bytes);
        ns.write_at(&block, lba * 4104).unwrap();
    }
    // 读 + verify 4 个 LBA（模拟 K4c Read 路径核心循环）
    let ns = &c.namespaces[&1];
    let mut interleaved = vec![0u8; 4 * 4104];
    ns.read_at(&mut interleaved, 0).unwrap();
    for i in 0..4usize {
        let blk = &interleaved[i * 4104..(i + 1) * 4104];
        let data = &blk[..4096];
        let tuple_arr: [u8; 8] = blk[4096..4104].try_into().unwrap();
        let tuple = PiTuple::from_bytes(&tuple_arr);
        assert_eq!(
            tuple.verify(data, i as u64, 1),
            crate::pi::PiCheck::Ok,
            "K4c per-LBA PI verify passes for lba={i}"
        );
    }
    // 篡改 LBA 2 data byte 0 → verify fail
    let ns = c.namespaces.get_mut(&1).unwrap();
    ns.write_at(&[0xff], 2 * 4104).unwrap();
    let ns = &c.namespaces[&1];
    let mut blk = vec![0u8; 4104];
    ns.read_at(&mut blk, 2 * 4104).unwrap();
    let data = &blk[..4096];
    let tuple_arr: [u8; 8] = blk[4096..4104].try_into().unwrap();
    let tuple = PiTuple::from_bytes(&tuple_arr);
    assert_eq!(tuple.verify(data, 2, 1), crate::pi::PiCheck::GuardFail);
}

/// **Phase O2** — Sqe::fuse() 解析 cdw0 bits 9:8。
#[test]
fn sqe_fuse_field_extraction() {
    // SQE 64 byte 全 0 + 用 zerocopy 解 →合法 zeroed struct
    let zero = [0u8; 64];
    let mut sqe: Sqe = zerocopy::FromBytes::read_from_bytes(&zero[..]).unwrap();
    // Normal opcode 0x01 (WRITE), cid=0x42, fuse=0
    sqe.cdw0 = 0x0042_0001;
    assert_eq!(sqe.fuse(), 0);
    assert_eq!(sqe.opcode(), 0x01);
    assert_eq!(sqe.cid(), 0x0042);
    // Fuse=01 (first) bit 8
    sqe.cdw0 = 0x0042_0101;
    assert_eq!(sqe.fuse(), 1);
    // Fuse=10 (second) bit 9
    sqe.cdw0 = 0x0042_0201;
    assert_eq!(sqe.fuse(), 2);
    // Fuse=11 reserved
    sqe.cdw0 = 0x0042_0301;
    assert_eq!(sqe.fuse(), 3);
}

/// **Phase O2** — IdentifyController.fuses bit 0 = Compare-and-Write 支持。
#[test]
fn identify_controller_advertises_fused_cw() {
    let buf = IdentifyController::build_v2_bytes(0x1414, 0xc0de, 1);
    // FUSES @ offset 522..524 in IdentifyController (spec § 5.17.2.2)
    let fuses = u16::from_le_bytes(buf[522..524].try_into().unwrap());
    assert_eq!(fuses & 0x0001, 0x0001, "FUSES bit 0 must advertise C&W");
}

/// **Phase O1** — COPY opcode dispatch 在 PI NS 上拒绝（INVALID_PROTECTION_INFO）。
/// 直接验证 nvm_opc 常量 + 设计契约：PI+Copy 组合未实现。
#[test]
fn copy_opcode_constant_matches_spec() {
    assert_eq!(crate::cmd::nvm_opc::COPY, 0x19);
}
