// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! W6b Task 1.3 irq 投递测：`pal_event::Event` 信号 → [`irq_wait_loop`] →
//! `Interrupt::deliver()` 累加计数。验证 eventfd 唤醒 → deliver 一对一，且循环
//! 在第一次信号后**重新 arm**（第二次信号仍能 deliver）。

use pal_async::DefaultPool;
use pal_async::task::Spawn;
use pal_async::timer::PolledTimer;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use vfio_user_pci_device::irq_wait_loop;
use vmcore::interrupt::Interrupt;

/// signal x2 → 计数 2，证明 wait 循环 deliver 一次后会 re-arm。
#[test]
fn irq_loop_delivers_and_rearms() {
    DefaultPool::run_with(async |driver| {
        let event = Event::new();
        let counter = Arc::new(AtomicU64::new(0));
        let c = counter.clone();
        let interrupt = Interrupt::from_fn(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });

        let waiter = PolledWait::new(&driver, event.clone()).expect("PolledWait");
        let task = driver.spawn("irq-loop", irq_wait_loop(waiter, interrupt));

        let mut timer = PolledTimer::new(&driver);

        // 第一次信号 → deliver 一次。
        event.signal();
        wait_for(&mut timer, &counter, 1).await;

        // 第二次信号 → 循环 re-arm → deliver 第二次。
        event.signal();
        wait_for(&mut timer, &counter, 2).await;

        // 收尾：drop task 取消 irq_wait_loop（本测同进程持 eventfd 的 dup，drop
        // 原 event 不会让 waiter 见到 EOF；生产中 firmware 关其端才会 → wait 返
        // Err → 循环 break，那条路径由 irq_wait_loop 自身保证）。
        drop(task);
    });
}

/// 轮询直到 counter 达到 `target`：用 timer sleep 让 executor park，使 reactor 有
/// 机会 service eventfd 并推进 irq_wait_loop task。带上限防卡死。
async fn wait_for(timer: &mut PolledTimer, counter: &Arc<AtomicU64>, target: u64) {
    for _ in 0..1000 {
        if counter.load(Ordering::Relaxed) >= target {
            return;
        }
        timer.sleep(Duration::from_millis(1)).await;
    }
    panic!("counter 未在期限内达到 {target}");
}
