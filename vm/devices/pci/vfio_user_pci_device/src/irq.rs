// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSI-X eventfd → guest 中断投递（W6b）。
//!
//! 每个 MSI-X 向量对应一个独立的 eventfd-wait 循环：firmware 触发该向量时
//! 写我方 eventfd → [`pal_async::wait::PolledWait`] 唤醒 → 调
//! [`vmcore::interrupt::Interrupt::deliver`] 把中断注入 guest（VTL0）。
//!
//! 设计要点：
//! - 这条 eventfd 是 **firmware → underhill 的独立 SCM_RIGHTS fd**，不走 control
//!   socket，因此与 worker 主循环（`from_device` / `recv_reply`）**不争用**。
//! - 每个向量一个独立 task（resolver 在 W6b Task 1.6 spawn），句柄存进 worker
//!   `_irq_tasks` 保活防 drop。
//! - `PolledWait::wait` 在唤醒时会以 8 字节 read **消费/清零** eventfd 计数，所以
//!   firmware 每发一次信号 → 唤醒一次 → 恰好 `deliver()` 一次（不丢不重）。

#![deny(unsafe_code)]

use pal_async::wait::PolledWait;
use pal_event::Event;
use vmcore::interrupt::Interrupt;

/// 一个 MSI-X 向量的 eventfd-wait 循环。
///
/// 持续 `wait().await`：每被 firmware 信号一次就 `deliver()` 一次；eventfd 关闭
/// （firmware 进程退出 / driver 消失）→ `wait` 返 `Err` → 循环退出。
///
/// `deliver` 是 fire-and-forget（不代表 guest 已实际收到），`Interrupt` 为
/// `Clone+Send`，从独立 task 调用安全。
pub async fn irq_wait_loop(mut waiter: PolledWait<Event>, interrupt: Interrupt) {
    // 每被 firmware 信号一次就 deliver 一次；eventfd 关闭（firmware 退出 / driver
    // 消失）→ `wait` 返 `Err` → `while let` 条件不再成立 → 循环退出。
    while let Ok(()) = waiter.wait().await {
        interrupt.deliver();
    }
}
