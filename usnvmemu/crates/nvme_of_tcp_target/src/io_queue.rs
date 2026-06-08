// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V5a** — session 端 IO queue 记账：admin Create IO CQ/SQ 已让
//! controller 内部 `cqs[qid] / sqs[qid]` 建好，session 这边镜像记下
//! "queue 已存在 + sq→cq 映射 + CQ sentinel"，供后续 Fabric Connect
//! qid≥1 校验 + IO CapsuleCmd 派发用。
//!
//! 状态机：Create IO CQ ⇒ insert `Cq{sentinel}`；Create IO SQ ⇒ insert
//! `Sq{cq_id}`；Fabric Connect qid=N ⇒ 找到对应 Sq 标 `connected=true`
//! + session.current_qid=N。

/// session 端单条 IO queue 状态。
#[derive(Debug, Clone)]
pub enum IoQueueState {
    /// IO Completion Queue：base GPA 用 session 自分配的 sentinel（>=
    /// CQ_BASE_GPA），controller post_cqe 时 `ctx.dma_write(sentinel, 16B)`
    /// 会被 session captured.writes drain 识别为 CQE write。
    Cq {
        /// CQ sentinel GPA（`cq_sentinel(qid)`）
        sentinel: u64,
        /// host Connect qid=N 是否已 ack
        connected: bool,
    },
    /// IO Submission Queue：指向某 CQ id。Fabric Connect 时校验。
    Sq {
        /// 对应的 CQ id（来自 Create IO SQ 的 cdw11 bits 31:16）
        cq_id: u16,
        /// host Connect qid=N 是否已 ack
        connected: bool,
    },
}

impl IoQueueState {
    /// Helper：构造一个 CQ entry（connected=false 初始）
    pub fn new_cq(sentinel: u64) -> Self {
        Self::Cq {
            sentinel,
            connected: false,
        }
    }
    /// Helper：构造一个 SQ entry（connected=false 初始）
    pub fn new_sq(cq_id: u16) -> Self {
        Self::Sq {
            cq_id,
            connected: false,
        }
    }

    /// 标 connected=true（Fabric Connect qid=N ack 后调）
    pub fn mark_connected(&mut self) {
        match self {
            IoQueueState::Cq { connected, .. } => *connected = true,
            IoQueueState::Sq { connected, .. } => *connected = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cq_default_not_connected() {
        let c = IoQueueState::new_cq(0xDEAD_BEEF);
        match c {
            IoQueueState::Cq {
                sentinel,
                connected,
            } => {
                assert_eq!(sentinel, 0xDEAD_BEEF);
                assert!(!connected);
            }
            _ => panic!("expected Cq"),
        }
    }

    #[test]
    fn sq_mark_connected_flips_flag() {
        let mut s = IoQueueState::new_sq(1);
        s.mark_connected();
        match s {
            IoQueueState::Sq { cq_id, connected } => {
                assert_eq!(cq_id, 1);
                assert!(connected);
            }
            _ => panic!("expected Sq"),
        }
    }
}
