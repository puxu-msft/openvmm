// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `worker.rs` 的单元测试。
//!
//! 拆分原因：父文件超 800 行（仓库 coding-style 上限）；测试分离让生产
//! 代码视图集中、IDE 跳转更快。所有测试共享 `super::*` 与 `test_swap_rx()`
//! 辅助函数。

use super::*;

/// 构造一个测试用 transport_swap channel：sender 立刻 forget 保活，
/// 让 worker 主循环（如果跑）不会因 sender drop 走 None arm。返回 receiver。
fn test_swap_rx() -> Receiver<crate::prepared::BoxedTransport> {
    let (tx, rx) = mesh::channel::<crate::prepared::BoxedTransport>();
    std::mem::forget(tx);
    rx
}

#[test]
fn dma_rate_allows_under_limit() {
    // 显式 64 MiB/s 限值，与 env 解耦：避免 CI 误设 OPENHCL_PCIE_REMOTE_DMA_BPS
    // 时 silent skip。
    let mut r = DmaRate::with_limit(64 * 1024 * 1024);
    assert!(r.try_consume(1024 * 1024));
    assert!(r.try_consume(1024 * 1024));
}

#[test]
fn dma_rate_rejects_over_limit() {
    let limit = 64 * 1024 * 1024;
    let mut r = DmaRate::with_limit(limit);
    assert!(r.try_consume(limit));
    assert!(!r.try_consume(1));
}

#[test]
fn dma_rate_resets_after_window() {
    let limit = 64 * 1024 * 1024;
    let mut r = DmaRate::with_limit(limit);
    assert!(r.try_consume(limit));
    // 不真等 1s — 直接构造已过期窗口
    r.window_start = Instant::now() - Duration::from_secs(2);
    assert!(r.try_consume(1024));
}

/// env override = 0 → 永远允许（rate limit disabled），仍受协议尺寸上限。
#[test]
fn dma_rate_env_zero_disables_limit() {
    let mut r = DmaRate::with_limit(0);
    // 巨量请求也允许
    assert!(r.try_consume(u64::MAX / 2));
    assert!(r.try_consume(u64::MAX / 2));
}

/// env override = 自定义阈值；模拟构造 limit=1MB 验证拒收路径。
#[test]
fn dma_rate_custom_limit_rejects_above() {
    let mut r = DmaRate::with_limit(1024 * 1024);
    assert!(r.try_consume(1024 * 1024));
    assert!(!r.try_consume(1));
}

/// inflight 计数器：MmioReadResult 处理后从 in_flight map 移除 → counter 减；
/// drain_in_flight 触发归零。
///
/// 不依赖真 guest driver — 用直接构造的 DeferredRead/Write token 模拟
/// device.rs → worker 的 DeviceRequest 流向（e2e 时 guest 无 driver 不
/// 会触发，这里单元测试覆盖该路径）。
#[test]
fn inflight_counter_tracks_in_flight_map() {
    use chipset_device::io::deferred::defer_read;
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use pcie_remote_protocol::MmioReadResult as Mrr;
    use pcie_remote_protocol::to_openhcl::Body;

    DefaultPool::run_with(|_| async move {
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let mut w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            Vec::new(),
            gm,
            stats,
            swap_rx,
        );

        // 手工把 2 个 InFlight 塞进 worker（模拟 device.rs 投递过来）。
        // _t1/_t2 是对应 host-side wait future；测试不 await 它们。
        let (_d1, _t1) = defer_read();
        let (_d2, _t2) = defer_read();
        w.in_flight.insert(
            10,
            InFlight::Read {
                token: _d1,
                access_size: 4,
            },
        );
        w.in_flight.insert(
            11,
            InFlight::Read {
                token: _d2,
                access_size: 8,
            },
        );
        // 模拟 device.rs 投递时 worker 主循环更新 stats：
        let cur = w.in_flight.len() as u64;
        w.stats.inflight_current.store(cur, Ordering::Relaxed);
        w.stats.inflight_peak.fetch_max(cur, Ordering::Relaxed);
        assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 2);
        assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);

        // host 回 seq=10 → in_flight 减 1，counter 更新
        let reply1 = ToOpenhcl {
            seq: 10,
            body: Some(Body::MmioReadResult(Mrr { value: 0xdead })),
        };
        assert!(w.dispatch_inbound(reply1).await);
        assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 1);
        // peak 保持 2（fetch_max 单调）
        assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);
        assert_eq!(stats_for_check.mmio_read_results.load(Ordering::Relaxed), 1);

        // drain（模拟 worker 进 Lost）→ inflight_current 归零
        w.drain_in_flight();
        assert_eq!(stats_for_check.inflight_current.load(Ordering::Relaxed), 0);
        // 但 peak 仍 2
        assert_eq!(stats_for_check.inflight_peak.load(Ordering::Relaxed), 2);
        assert!(w.in_flight.is_empty());
    });
}

/// 验证 InterruptFire dispatch 的边界：
/// - 合法 msix_index 在范围内 → 不增 bad-frame 计数
/// - 越界 msix_index → 增 bad-frame 计数
///
/// 用 MsiTarget::disconnected() 构造 MsixEmulator；deliver() 是 no-op。
/// 用 futures::io::Cursor 当 "transport"（async read/write 兼容）。
/// 实际帧不通过 cursor 流，只直接调 dispatch_inbound。
#[test]
fn interrupt_fire_bounds() {
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use pci_core::capabilities::msix::MsixEmulator;
    use pci_core::msi::MsiTarget;
    use pcie_remote_protocol::InterruptFire;
    use pcie_remote_protocol::to_openhcl::Body;

    DefaultPool::run_with(|_| async move {
        let target = MsiTarget::disconnected();
        let (msix, _cap) = MsixEmulator::new(4, 2, &target);
        let interrupts = (0..2)
            .map(|i| msix.interrupt(i).unwrap())
            .collect::<Vec<_>>();

        // 内存 cursor 作 transport；本测试不真走 transport，只测 dispatch_inbound。
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let mut w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            interrupts,
            gm,
            stats,
            swap_rx,
        );

        // msix_index = 0 valid。
        let ok_msg = ToOpenhcl {
            seq: 1,
            body: Some(Body::InterruptFire(InterruptFire { msix_index: 0 })),
        };
        assert!(w.dispatch_inbound(ok_msg).await);
        assert_eq!(w.consecutive_bad_frames, 0);
        assert_eq!(stats_for_check.interrupts_fired.load(Ordering::Relaxed), 1);

        // msix_index = 1 valid。
        let ok_msg2 = ToOpenhcl {
            seq: 2,
            body: Some(Body::InterruptFire(InterruptFire { msix_index: 1 })),
        };
        assert!(w.dispatch_inbound(ok_msg2).await);
        assert_eq!(w.consecutive_bad_frames, 0);
        assert_eq!(stats_for_check.interrupts_fired.load(Ordering::Relaxed), 2);
        assert_eq!(stats_for_check.interrupts_oob.load(Ordering::Relaxed), 0);

        // msix_index = 2 → out of bounds → bad-frame +1，仍 < 阈值。
        let oob = ToOpenhcl {
            seq: 3,
            body: Some(Body::InterruptFire(InterruptFire { msix_index: 2 })),
        };
        assert!(w.dispatch_inbound(oob).await);
        assert_eq!(w.consecutive_bad_frames, 1);
        assert_eq!(stats_for_check.interrupts_oob.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            1
        );

        // 累计到 MAX_BAD_FRAMES 应让 dispatch_inbound 返回 false。
        for i in 0..(MAX_BAD_FRAMES - 1) {
            let bad = ToOpenhcl {
                seq: 100 + i as u64,
                body: Some(Body::InterruptFire(InterruptFire { msix_index: 99 })),
            };
            let cont = w.dispatch_inbound(bad).await;
            if i < (MAX_BAD_FRAMES - 2) {
                assert!(cont, "i={i} should continue");
            } else {
                assert!(!cont, "i={i} should stop (达阈值)");
            }
        }
        // 总越界数：1 (msix_index=2) + (MAX_BAD_FRAMES-1) 个 99
        assert_eq!(
            stats_for_check.interrupts_oob.load(Ordering::Relaxed),
            1 + (MAX_BAD_FRAMES - 1) as u64
        );
    });
}

/// DMA ReadGpa bounds check: len=0 / len > MAX_DMA_BYTES 应被拒绝 +
/// 计为 bad-frame；合法 len 走 GuestMemory.read_at（空 GuestMemory 上会
/// 失败但不算 bad-frame，read_gpa_requests 仍 ++）。
#[test]
fn read_gpa_bounds_and_stats() {
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use pcie_remote_protocol::ReadGpaRequest;

    DefaultPool::run_with(|_| async move {
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let mut w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            Vec::new(),
            gm,
            stats,
            swap_rx,
        );

        // len=0 → bounds reject + record_bad
        let req0 = ReadGpaRequest {
            token: 1,
            gpa: 0,
            len: 0,
        };
        let _ = w.handle_read_gpa(req0).await;
        assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 1);
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            1
        );

        // len 太大 → bounds reject + record_bad
        let req_big = ReadGpaRequest {
            token: 2,
            gpa: 0,
            len: (MAX_DMA_BYTES + 1) as u32,
        };
        let _ = w.handle_read_gpa(req_big).await;
        assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 2);
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            2
        );

        // 合法 len（4 字节）→ guest_memory.read_at 在空 mem 上失败 →
        // reply ok=false 不计 bad-frame；read_gpa_requests 仍 ++；
        // consecutive_bad_frames 归零
        let req_ok = ReadGpaRequest {
            token: 3,
            gpa: 0,
            len: 4,
        };
        let _ = w.handle_read_gpa(req_ok).await;
        assert_eq!(stats_for_check.read_gpa_requests.load(Ordering::Relaxed), 3);
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            0
        );
    });
}

/// WriteGpa 同样 bounds 检查 + stats 计数。
#[test]
fn write_gpa_bounds_and_stats() {
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use pcie_remote_protocol::WriteGpaRequest;

    DefaultPool::run_with(|_| async move {
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let mut w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            Vec::new(),
            gm,
            stats,
            swap_rx,
        );

        // 空 data → bounds reject + record_bad
        let req0 = WriteGpaRequest {
            token: 1,
            gpa: 0,
            data: vec![],
        };
        let _ = w.handle_write_gpa(req0).await;
        assert_eq!(
            stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            1
        );

        // data 超大 → bounds reject
        let req_big = WriteGpaRequest {
            token: 2,
            gpa: 0,
            data: vec![0u8; MAX_DMA_BYTES + 1],
        };
        let _ = w.handle_write_gpa(req_big).await;
        assert_eq!(
            stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
            2
        );
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            2
        );

        // 合法 → guest_memory.write_at fail (empty mem) → ok=false 不 bad
        let req_ok = WriteGpaRequest {
            token: 3,
            gpa: 0,
            data: vec![0xff; 8],
        };
        let _ = w.handle_write_gpa(req_ok).await;
        assert_eq!(
            stats_for_check.write_gpa_requests.load(Ordering::Relaxed),
            3
        );
        assert_eq!(
            stats_for_check
                .consecutive_bad_frames
                .load(Ordering::Relaxed),
            0
        );
    });
}

/// 验证 DmaRate 与 dma_rate_limit_rejects 计数器配合：64 个 64KB
/// 全允许（== 4 MiB << 64 MiB/s）；同窗口再 1024 个 → 第 1025 起拒。
/// 间接也是 K-NEW-C e2e stress 模式的 unit-test 等价物。
#[test]
fn dma_rate_counter_increments_on_reject() {
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use pcie_remote_protocol::ReadGpaRequest;

    DefaultPool::run_with(|_| async move {
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let mut w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            Vec::new(),
            gm,
            stats,
            swap_rx,
        );

        // 此测试需要默认 64 MiB/s 阈值；如果 env 污染则 fail-loud（不
        // silent skip，避免 CI 误以为通过）。
        let limit = w.dma_rate.limit_bps;
        assert_eq!(
            limit,
            64 * 1024 * 1024,
            "test requires default DMA limit; unset OPENHCL_PCIE_REMOTE_DMA_BPS"
        );

        // 发 1100 个 64KB ReadGpa：1024 个允许（占满 64 MiB/s）+ 76 个被拒
        for i in 0..1100u64 {
            let req = ReadGpaRequest {
                token: i,
                gpa: 0,
                len: 65536,
            };
            let _ = w.handle_read_gpa(req).await;
        }
        assert_eq!(
            stats_for_check.read_gpa_requests.load(Ordering::Relaxed),
            1100
        );
        // 1100 - 1024 = 76 被 rate limit 拒绝
        assert_eq!(
            stats_for_check
                .dma_rate_limit_rejects
                .load(Ordering::Relaxed),
            76
        );
    });
}

/// record_lost 写入 unix-ms 时间戳 + reason bit；record_revive 增 revive_count
/// + 更新时间戳。验证 inspect 暴露字段被 worker 正确更新。
///
/// 不用真 spawn worker — 直接用 `Worker::new` 构造然后调 `record_lost` /
/// `record_revive` 私有方法（test 在同 crate 内可访问）。
#[test]
fn lost_revive_timestamps_and_reason() {
    use crate::worker::lost_reason;
    use futures::io::Cursor;
    use pal_async::DefaultPool;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    DefaultPool::run_with(|_| async move {
        let cursor = Cursor::new(Vec::<u8>::new());
        let (_dev_tx, dev_rx) = mesh::channel::<DeviceRequest>();
        let state = SharedState::new(DeviceState::Live);
        let gm = GuestMemory::empty();
        let stats: SharedWorkerStats = Arc::new(WorkerStats::default());
        let stats_for_check = stats.clone();
        let swap_rx = test_swap_rx();
        let w = Worker::new(
            Box::new(cursor) as crate::prepared::BoxedTransport,
            state,
            dev_rx,
            Vec::new(),
            gm,
            stats,
            swap_rx,
        );

        // 初始：全 0
        assert_eq!(stats_for_check.last_lost_at_ms.load(Ordering::Relaxed), 0);
        assert_eq!(stats_for_check.last_revive_at_ms.load(Ordering::Relaxed), 0);
        assert_eq!(stats_for_check.revive_count.load(Ordering::Relaxed), 0);
        assert_eq!(stats_for_check.last_lost_reason.load(Ordering::Relaxed), 0);

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        w.record_lost(lost_reason::READ_ERR);
        let lost_ms = stats_for_check.last_lost_at_ms.load(Ordering::Relaxed);
        assert!(
            lost_ms >= before,
            "lost_ms {lost_ms} not after before {before}"
        );
        assert_eq!(
            stats_for_check.last_lost_reason.load(Ordering::Relaxed),
            lost_reason::READ_ERR
        );

        // 第二次 Lost 覆盖 reason（位标记 store-only，仅最后一次）
        w.record_lost(lost_reason::WRITE_ERR | lost_reason::DISPATCH_FAIL);
        assert_eq!(
            stats_for_check.last_lost_reason.load(Ordering::Relaxed),
            lost_reason::WRITE_ERR | lost_reason::DISPATCH_FAIL
        );

        // record_revive：++ count + 时间戳
        w.record_revive();
        assert_eq!(stats_for_check.revive_count.load(Ordering::Relaxed), 1);
        let r1 = stats_for_check.last_revive_at_ms.load(Ordering::Relaxed);
        assert!(r1 >= before);

        w.record_revive();
        assert_eq!(stats_for_check.revive_count.load(Ordering::Relaxed), 2);
    });
}
