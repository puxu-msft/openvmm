// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dead-man switch（spec §3.5）。
//!
//! 双条件并联（任一触发即返回 true 让调用方进 Lost）：
//! 1. 连续 ≥ `N_CONSEC` 次 timeout
//! 2. 滑窗 `WINDOW` 内 ≥ `N_WINDOW` 次 timeout 且占比 ≥ 50%
//!
//! Connecting 阶段调用方不应记录 timeout（boot 早期 host 可能未就绪）。

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

/// 连续 timeout 阈值。
pub const N_CONSEC: u32 = 4;
/// 滑窗 timeout 计数阈值。
pub const N_WINDOW: usize = 8;
/// 滑窗时长。
pub const WINDOW: Duration = Duration::from_secs(1);

/// 双条件 dead-man switch。
pub struct DeadMan {
    consec: u32,
    window: VecDeque<(Instant, bool /* is_timeout */)>,
}

impl DeadMan {
    /// 构造。
    pub fn new() -> Self {
        Self {
            consec: 0,
            window: VecDeque::with_capacity(16),
        }
    }

    /// 记录一次 timeout；返回 true 表示已触发（调用方应进 Lost）。
    pub fn record_timeout(&mut self, now: Instant) -> bool {
        self.consec += 1;
        self.window.push_back((now, true));
        self.trim(now);
        self.is_tripped()
    }

    /// 记录一次 success；重置 consec 计数。返回 false（success 永不触发）。
    pub fn record_success(&mut self, now: Instant) -> bool {
        self.consec = 0;
        self.window.push_back((now, false));
        self.trim(now);
        false
    }

    fn trim(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.window.front() {
            if now.duration_since(t) > WINDOW {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_tripped(&self) -> bool {
        if self.consec >= N_CONSEC {
            return true;
        }
        let timeouts = self.window.iter().filter(|(_, t)| *t).count();
        timeouts >= N_WINDOW && timeouts * 2 >= self.window.len()
    }
}

impl Default for DeadMan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consec_threshold() {
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(!d.record_timeout(t0 + Duration::from_millis(i)));
        }
        assert!(d.record_timeout(t0 + Duration::from_millis(4)));
    }

    #[test]
    fn one_success_resets_consec() {
        let mut d = DeadMan::new();
        let now = Instant::now();
        for _ in 0..3 {
            d.record_timeout(now);
        }
        d.record_success(now);
        for _ in 0..3 {
            assert!(!d.record_timeout(now));
        }
    }

    #[test]
    fn window_ratio_only_no_consec() {
        // 不连续 timeout（穿插 success），只靠滑窗 ratio + 数量触发。
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        let mut tripped = false;
        // 16 次循环：交替 timeout/success → consec 永远 ≤1，但滑窗 ratio 50%
        for i in 0..16 {
            let when = t0 + Duration::from_millis(i * 10);
            let res = if i % 2 == 0 {
                d.record_timeout(when)
            } else {
                d.record_success(when)
            };
            if res {
                tripped = true;
                break;
            }
        }
        // 8 个 timeout（== N_WINDOW），ratio 50% → trip
        assert!(tripped, "ratio threshold should trip");
    }

    #[test]
    fn old_entries_trimmed() {
        let mut d = DeadMan::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            d.record_timeout(t0);
        }
        d.record_success(t0);
        let t1 = t0 + Duration::from_secs(2);
        for _ in 0..3 {
            d.record_timeout(t1);
        }
        assert!(d.record_timeout(t1 + Duration::from_millis(1)));
    }
}
