// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-3** — async session pump 骨架。
//!
//! 本 module 是 V8e-3 主交付物：提供 [`accept_and_handshake_async`] +
//! [`AsyncSession::pump_one_async`] 让 bin 可以走纯 tokio 路径（V8e-4/5/6 在
//! 此基础上加 AER Notify / KATO timer / e2e）。
//!
//! # 与 sync [`V2Session`](crate::V2Session) 双轨并存
//!
//! V8e-3 不删 sync V2Session（30+ V8b/c/d/f 集成测试用 `std::net::TcpStream` +
//! `accept_and_handshake_shared`）。本 module 是**并行**新加 surface：
//! - sync 测试 / 老 V2Session 仍跑 `parking_lot::Mutex` 短锁 + thread::spawn
//! - 新 async bin (V8e-2) + 真并发 IO (V8e-6) 走 [`AsyncSession`]
//!
//! 共享同一 [`SharedControllerInner`](crate::SharedControllerInner)（含
//! `parking_lot::Mutex<NvmeController>` + per-conn token slab + per-conn id
//! 分配器），所以两路 conn 可以**同时**跟同一 controller 实例交互。
//!
//! # 锁选择 (plan §3 Q1 决策)
//!
//! controller 字段保持 `parking_lot::Mutex` 而非切 `tokio::sync::Mutex`：
//! - 教学版 closure 内调用都是 cheap 短锁（dispatch / install_admin_cq /
//!   nvme_pending_aer_count_for_conn 等），不跨 await
//! - `with_controller` API closure-only 设计 + crate-级 `#![deny(clippy::await_holding_lock)]`
//!   编译期防滥用
//! - parking_lot 性能 / 跨平台 / 无 poisoning 在短锁场景更合适
//!
//! 真正需要 `tokio::sync::Mutex` 是"持锁跨 await" 场景；V8e plan R-2 强调
//! 这种用法应通过 R2T 三段式重构（lock-pop → unlock-await wire → lock-complete）
//! 避免，而不是换锁。`tokio::sync::Mutex` 留 V-followup（如未来加 async
//! controller backend 需要）。
//!
//! # KATO 超时
//!
//! sync 路径靠 `set_read_timeout` + `FramingError::ReadTimeout`；async 路径靠
//! caller-side `tokio::time::timeout(read_pdu_async, kato_deadline)` 或 select!
//! 第 N arm。本 module **只暴露**读 PDU + 短锁 dispatch 骨架，KATO timer 在
//! V8e-5 加。

use crate::SharedController;
use crate::framing::{Pdu, read_pdu_async, write_pdu_async};
use crate::pdu::{CommonHdr, IcPsh, pdu_type};
use anyhow::Context as _;
use std::pin::Pin;
use std::time::Duration;
use tokio::net::TcpStream as TokioStream;
use tokio::time::{Instant as TokioInstant, Sleep, sleep_until};
use zerocopy::IntoBytes;

/// **V8e-3** — async session：握手完成后通过 [`pump_one_async`] 接 PDU
/// 直到 shutdown 或 peer close。
///
/// 字段集合刻意收得比 sync `V2Session` 少：V8e-3 只交付 ICReq/Connect/简单
/// dispatch 骨架；V8e-4 加 AER Notify、V8e-5 加 KATO Sleep、V8e-6 加完整
/// admin/IO dispatch。
pub struct AsyncSession {
    stream: TokioStream,
    controller: SharedController,
    /// 协商后参数（与 sync `NegotiatedIc` 等价）。
    pub negotiated: crate::NegotiatedIc,
    /// per-conn 标签（V8c per-conn AER 路由 + Drop cleanup 必备）。
    pub conn_id: u32,
    /// per-conn token slab base（V8b reviewer C-1 防多 conn 撞 pending_ios key）。
    pub next_token: u64,
    /// discovery_mode 由 handshake 入口 derive。
    pub discovery_mode: bool,
    /// **V8e-4** — controller-wide AER wakeup notify clone（plan §3 Q3）。
    /// session select! 用 `aen_notify.notified().await` 替代 sync 100ms tick；
    /// 唤醒后自检 `nvme_pending_aer_count_for_conn(conn_id)` 过滤 spurious
    /// wakeup。
    pub aen_notify: std::sync::Arc<tokio::sync::Notify>,
    /// **V8e-5** — Keep-Alive Timeout (spec § 7.13)。Connect 成功后由 admin
    /// cmd handler 调 [`Self::set_kato`] 注入；0 = 禁用 timer（spec § 7.13
    /// "Keep Alive disabled"）。
    pub kato_tmo: Duration,
    /// **V8e-5** — 下一次 KATO 超时时刻。`Some(Pin<Box<Sleep>>)` 已 arm；
    /// `None` = KATO 禁用（kato_tmo=0）或 Connect 未完成。每次 admin/IO cmd
    /// 入口调 [`Self::reset_kato_deadline`] 原地 `Pin::as_mut.reset` 刷新。
    ///
    /// 设计依据：plan §3 Q2 — Sleep 比 Interval 语义更贴 deadline；Pin<Box>
    /// 让 reset 不破坏已有 future state。
    kato_deadline: Option<Pin<Box<Sleep>>>,
}

/// **V8e-3** — async 版 ICReq/ICResp handshake。语义与 sync
/// [`crate::ic_handshake`] 1:1 等价（HDGST/DDGST 协商写死禁用；MAXH2CDATA
/// = `MAXH2CDATA_BYTES`）。byte-stream 等价已被 V8e-1 regression gate 覆盖。
pub async fn ic_handshake_async(stream: &mut TokioStream) -> anyhow::Result<crate::NegotiatedIc> {
    // 1. recv ICReq
    let pdu = read_pdu_async(stream).await.context("V8e-3: read ICReq")?;
    if pdu.header.pdu_type != pdu_type::ICREQ {
        anyhow::bail!(
            "V8e-3: expected ICReq, got pdu_type={:#x}",
            pdu.header.pdu_type
        );
    }
    let icreq: IcPsh = crate::pdu::decode_psh(&pdu.psh).context("V8e-3: decode ICReq PSH")?;
    let pfv = icreq.pfv;
    let maxr2t = icreq.maxr2t_or_maxh2cdata;
    if pfv != 0 {
        anyhow::bail!("V8e-3: ICReq PFV={pfv} unsupported (only 0)");
    }
    // 2. send ICResp — 与 sync `ic_handshake` byte-stream 等价（V8e-1 regression
    // gate `v8e1_sync_vs_async_serialize_bytes_identical` 锁定 serialize_pdu 一致）
    let resp_hdr = CommonHdr {
        pdu_type: pdu_type::ICRESP,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let resp_psh = IcPsh {
        pfv: 0,
        hpda_or_cpda: 0,
        digest: 0,
        // ICResp 的 `maxh2cdata` 字段是 u32 PDU offset bits；这里写 0 让 host
        // fallback 默认值（与 sync ic_handshake 当前实现一致；V8e-6 完整 Connect
        // 路径会真正暴露 MAXH2CDATA_BYTES 给 host）
        maxr2t_or_maxh2cdata: 0,
        rsvd: [0u8; 112],
    };
    write_pdu_async(stream, &resp_hdr, resp_psh.as_bytes(), &[])
        .await
        .context("V8e-3: write ICResp")?;
    Ok(crate::NegotiatedIc {
        pfv: 0,
        hpda: 0,
        hdgst: false,
        ddgst: false,
        maxh2cdata: crate::MAXH2CDATA_BYTES,
        maxr2t: maxr2t as u32,
    })
}

/// **V8e-3** — async session handshake：ICReq/ICResp + 共 controller 注入
/// admin CQ + 分配 token slab + conn_id。语义与 sync
/// [`V2Session::accept_and_handshake_shared`](crate::V2Session::accept_and_handshake_shared)
/// 等价（共享 `SharedControllerInner` 同 atomic 计数器）。
pub async fn accept_and_handshake_async(
    mut stream: TokioStream,
    controller: SharedController,
) -> anyhow::Result<AsyncSession> {
    // **V7 / V8b** — derive discovery mode 于 handshake 前。短锁 read。
    let discovery_mode = controller.controller.lock().nvme_is_discovery_mode();
    let negotiated = ic_handshake_async(&mut stream).await?;
    // **Phase V3 / V8b** — 给 controller 装一个"假" admin CQ（idempotent）。
    {
        let mut c = controller.controller.lock();
        c.nvme_install_admin_cq(crate::CQ_BASE_GPA, crate::ADMIN_CQ_SIZE)
            .context("V8e-3: admin CQ install (multi-conn share params mismatch)")?;
    }
    let next_token = controller.allocate_token_slab();
    let conn_id = controller.allocate_conn_id();
    let aen_notify = controller.aen_notify_handle();
    Ok(AsyncSession {
        stream,
        controller,
        negotiated,
        conn_id,
        next_token,
        discovery_mode,
        aen_notify,
        kato_tmo: Duration::from_secs(0),
        kato_deadline: None,
    })
}

impl AsyncSession {
    /// **V8e-3 / V8e-4 / V8e-5** — async 主循环单次 tick。
    ///
    /// 4 arm（V8e-6 后 dispatch 路径填充 PDU 处理）：
    /// 1. `shutdown.changed()` → 返 `Shutdown` 让 caller 退 loop
    /// 2. `aen_notify.notified()` → 返 `AenReady{pending}` 让 caller drain AER
    /// 3. `kato_deadline` 触发 (kato_tmo>0 时) → 返 `KatoExpired` 让 caller 关 conn
    /// 4. `read_pdu_async` → 返 `Pdu(pdu)` 让 caller dispatch
    ///
    /// 注意：`PumpEvent::Pdu(_)` 触发后 caller 应在真 dispatch 完成后调
    /// [`Self::reset_kato_deadline`]（V8e-6 admin/IO handler 入口逻辑）。
    pub async fn pump_one_async(
        &mut self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<PumpEvent> {
        // V8e-5：KATO=0 时第 3 arm 用 `std::future::pending()` 占位（spec § 7.13
        // "Keep Alive disabled"）。已 arm 的 Sleep 通过 `as_mut` 拿 Pin 借用。
        let kato_fut = async {
            match self.kato_deadline.as_mut() {
                Some(s) => s.as_mut().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                tracing::info!(conn_id = self.conn_id, "V8e-3 session shutdown");
                Ok(PumpEvent::Shutdown)
            }
            _ = self.aen_notify.notified() => {
                // V8e-4 spurious wakeup 过滤：自检本 conn 是否真有 pending AER
                // 才让 caller drain。读 controller 短锁 (parking_lot 不跨 await)。
                let cnt = self.controller.controller.lock()
                    .nvme_pending_aer_count_for_conn(self.conn_id);
                tracing::debug!(
                    conn_id = self.conn_id,
                    pending = cnt,
                    "V8e-4 AER notify woke session"
                );
                Ok(PumpEvent::AenReady { pending: cnt })
            }
            _ = kato_fut => {
                tracing::warn!(
                    conn_id = self.conn_id,
                    kato_ms = self.kato_tmo.as_millis(),
                    "V8e-5 KATO expired"
                );
                Ok(PumpEvent::KatoExpired)
            }
            r = read_pdu_async(&mut self.stream) => {
                match r {
                    Ok(pdu) => Ok(PumpEvent::Pdu(pdu)),
                    Err(e) => {
                        if let Some(crate::framing::FramingError::PeerClosed { .. }) =
                            e.downcast_ref::<crate::framing::FramingError>()
                        {
                            Ok(PumpEvent::PeerClosed)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
        }
    }

    /// **V8e-5** — caller (Connect handler 完成 / V8e-6 admin/IO dispatch
    /// 入口) 注入 KATO 超时；spec § 7.13 KATO 字段单位 ms。
    /// `kato_ms = 0` 表示 disable（spec 允许）。
    pub fn set_kato(&mut self, kato_ms: u32) {
        self.kato_tmo = Duration::from_millis(u64::from(kato_ms));
        if self.kato_tmo.is_zero() {
            self.kato_deadline = None;
        } else {
            self.kato_deadline = Some(Box::pin(sleep_until(TokioInstant::now() + self.kato_tmo)));
        }
        tracing::info!(
            conn_id = self.conn_id,
            kato_ms = self.kato_tmo.as_millis(),
            "V8e-5 KATO armed"
        );
    }

    /// **V8e-5** — admin/IO cmd handler 入口调，把 KATO deadline 原地刷到
    /// `now + kato_tmo`（plan §3 Q2: Pin::as_mut.reset 不破坏 select! 拿到的
    /// future borrow）。kato_tmo=0 时 no-op。
    pub fn reset_kato_deadline(&mut self) {
        if self.kato_tmo.is_zero() {
            return;
        }
        let new_deadline = TokioInstant::now() + self.kato_tmo;
        if let Some(s) = self.kato_deadline.as_mut() {
            s.as_mut().reset(new_deadline);
        } else {
            self.kato_deadline = Some(Box::pin(sleep_until(new_deadline)));
        }
    }

    /// 暴露 controller 引用供测试 / V8e-4..6 callsite 用。
    pub fn controller(&self) -> &SharedController {
        &self.controller
    }

    /// 暴露 next_token 让 V8e-6 后 dispatch path 接续 V8b token slab。
    pub fn next_token(&self) -> u64 {
        self.next_token
    }

    /// **V8e-5 test-only** — 暴露 kato_tmo 给测试断言。
    pub fn kato_tmo(&self) -> Duration {
        self.kato_tmo
    }
}

/// **V8e-3 / V8e-4 / V8e-5** — `pump_one_async` 单次 tick 的结果。
///
/// caller 行动：
/// - `Shutdown` / `PeerClosed` / `KatoExpired` 都应退 loop
/// - `AenReady` 应 drain AER + 调 fire_aen_for_conn wire emit
/// - `Pdu(pdu)` 应 dispatch（V8e-6 后 admin/IO handler）+ 调 reset_kato_deadline
#[derive(Debug)]
pub enum PumpEvent {
    /// shutdown signal 触发，session 应退 loop。
    Shutdown,
    /// peer close stream（EOF），session 应退 loop。
    PeerClosed,
    /// **V8e-5** — KATO 超时（spec § 7.13），session 应关 conn。
    KatoExpired,
    /// AER wakeup 触发，session 应 drain；`pending` 是本 conn pending AER 数。
    AenReady {
        /// 本 conn 在 controller 端的 pending AER 计数（spurious wakeup 时 = 0）。
        pending: usize,
    },
    /// 收到一帧 PDU 待 dispatch。
    Pdu(Pdu),
}

/// **V8c (security-reviewer H-1) + V8d** — conn drop 时清理 controller 端
/// 本 conn 残留的 pending AER（与 sync `V2Session::drop` 等价）。
/// V8e-3 `AsyncSession` 暂不持 IO queue 镜像（V8e-6 后加 dispatch 才有此
/// 状态），故此处仅 AER cleanup；V8e-6 后补 IO queue sweep。
impl Drop for AsyncSession {
    fn drop(&mut self) {
        if self.conn_id == 0 {
            return;
        }
        let conn_id = self.conn_id;
        let shared = std::sync::Arc::clone(&self.controller);
        let aer_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            shared.controller.lock().nvme_cleanup_conn_aers(conn_id)
        }));
        match aer_result {
            Ok(n) if n > 0 => {
                tracing::info!(conn_id, cleaned_aer = n, "V8e-3 AsyncSession Drop");
            }
            Ok(_) => {}
            Err(_) => tracing::error!(
                conn_id,
                "V8e-3 AsyncSession Drop: AER cleanup panic (suppressed)"
            ),
        }
    }
}
