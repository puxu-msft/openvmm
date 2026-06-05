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
use crate::fabric::{self, ConnectData, fabric_sc, fctype};
use crate::framing::{Pdu, read_pdu_async, write_pdu_async};
use crate::pdu::{CommonHdr, IcPsh, pdu_type};
use anyhow::Context as _;
use std::pin::Pin;
use std::time::Duration;
use tokio::net::TcpStream as TokioStream;
use tokio::time::{Instant as TokioInstant, Sleep, sleep_until};
use zerocopy::{FromBytes, IntoBytes};

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
    /// **V8e-5** — Keep-Alive Timeout (spec § 7.13)。Connect 成功后由
    /// handle_connect_async 注入；0 = 禁用 timer。
    pub kato_tmo: Duration,
    /// **V8e-5** — 下一次 KATO 超时时刻。
    kato_deadline: Option<Pin<Box<Sleep>>>,

    // ============ V8e-7-2 新增：完整 dispatch 所需 session 端 state ============
    /// **V8e-7-2** — Connect 分配的 CNTLID（写死 `fabric::TEACHING_CNTLID`；
    /// 与 sync V2Session 等价）。
    pub cntlid: u16,
    /// **V8e-7-2** — admin queue 是否已 Connect (qid=0)；第二次 Connect qid=0 应拒。
    pub admin_connected: bool,
    /// **V8e-7-2** — 当前 SQ id（V5a 单 conn 单 IO SQ 模型；0 = admin）。
    pub current_qid: u16,
    /// **V8e-7-2** — IO queue 镜像（Create IO CQ/SQ 后填）。
    pub io_queues: std::collections::HashMap<u16, crate::io_queue::IoQueueState>,
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
        cntlid: fabric::TEACHING_CNTLID,
        admin_connected: false,
        current_qid: 0,
        io_queues: std::collections::HashMap::new(),
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
///
/// **V8e-7-2** — Drop 扩展也 sweep 本 conn `io_queues` 镜像里的 IO SQ/CQ
/// （与 sync V2Session V8d Drop 1:1 等价；spec § 7.6.1 ordering 先 SQ 后 CQ）。
/// peer close 路径（不发 Disconnect）也走这里兜底。
impl Drop for AsyncSession {
    fn drop(&mut self) {
        if self.conn_id == 0 {
            return;
        }
        let conn_id = self.conn_id;
        let qids: Vec<u16> = self.io_queues.keys().copied().filter(|&q| q != 0).collect();
        let shared = std::sync::Arc::clone(&self.controller);
        // V8d M-3：拆两 catch_unwind 防 AER cleanup panic 阻塞 IO queue sweep
        let aer_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let shared = std::sync::Arc::clone(&shared);
            move || shared.controller.lock().nvme_cleanup_conn_aers(conn_id)
        }));
        let qids2 = qids.clone();
        let sweep_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut c = shared.controller.lock();
            for &q in &qids2 {
                c.nvme_delete_io_sq(q);
            }
            for &q in &qids2 {
                c.nvme_delete_io_cq(q);
            }
            qids2.len()
        }));
        match (aer_result, sweep_result) {
            (Ok(cleaned_aer), Ok(n_qids)) if cleaned_aer > 0 || n_qids > 0 => {
                tracing::info!(
                    conn_id,
                    cleaned_aer,
                    n_qids,
                    "V8e-7-2 AsyncSession Drop: cleaned conn state"
                );
            }
            (Ok(_), Ok(_)) => {}
            (Err(_), _) => tracing::error!(
                conn_id,
                "V8e-7-2 AsyncSession Drop: AER cleanup panic (suppressed)"
            ),
            (_, Err(_)) => tracing::error!(
                conn_id,
                "V8e-7-2 AsyncSession Drop: IO queue sweep panic (suppressed)"
            ),
        }
    }
}

// ============ V8e-7-2: dispatch_pdu_async + fabric handlers ============

/// **V8e-7-2 / V8e-7-3** — `dispatch_pdu_async` 单次 PDU dispatch 的结果。
///
/// caller (handle_conn_async) 应：
/// - `disconnected: true` → break loop（spec § 3.5 Disconnect 真清后正常关）
/// - `disconnected: false` → 继续下次 pump_one_async
#[derive(Debug, Default, Clone, Copy)]
pub struct DispatchOutcome {
    /// 是否本 PDU 是 Disconnect 命令（已 ACK + 已清 IO queues + caller 应 break）。
    pub disconnected: bool,
}

impl AsyncSession {
    /// **V8e-7-2** — async PDU dispatch 入口。
    ///
    /// 行为与 sync `dispatch_capsule_cmd` 1:1 等价，但走 tokio async 路径
    /// （`write_pdu_async` / `read_pdu_async`）。当前 V8e-7-2 实现 fabric 全
    /// 部 (Connect/PropertyGet/PropertySet/Disconnect)；admin/IO 命令的完整
    /// dispatch 在 V8e-7-3 继续补（与 sync V2Session handle_admin_cmd /
    /// handle_io_cmd 对齐 + sync vs async byte-identical regression gate）。
    ///
    /// **V8e-7-2 reviewer R-2** — KATO reset 在函数体顶**统一调一次**，避免
    /// 漏点（spec § 7.13 任一 cmd 都算 keep-alive；AER 排除性 minor）。
    pub async fn dispatch_pdu_async(&mut self, pdu: Pdu) -> anyhow::Result<DispatchOutcome> {
        // **V8e-7-2 R-2** — KATO reset
        self.reset_kato_deadline();

        // **V8e-7-1 决策核** — 用 dispatch_plan 分类
        let state = self.state_snapshot();
        match crate::dispatch_plan::decide_capsule_kind(&pdu.psh, state) {
            crate::dispatch_plan::CapsuleKind::PshTooShort { psh_len } => {
                self.send_c2h_term_async(crate::pdu::term_fes::INVALID_PDU_HDR)
                    .await?;
                anyhow::bail!("V8e-7-2 CapsuleCmd PSH too short: {psh_len} < 64");
            }
            crate::dispatch_plan::CapsuleKind::Fabric { cid } => {
                let sqe = &pdu.psh[..64];
                let ft = match fabric::sqe_fctype(sqe) {
                    Ok(t) => t,
                    Err(_) => {
                        self.send_capsule_resp_err_async(cid, 0x02).await?;
                        return Ok(DispatchOutcome::default());
                    }
                };
                match ft {
                    fctype::CONNECT => {
                        self.handle_connect_async(cid, sqe, &pdu.data).await?;
                        Ok(DispatchOutcome::default())
                    }
                    fctype::PROPERTY_GET => {
                        self.handle_property_get_async(cid, sqe).await?;
                        Ok(DispatchOutcome::default())
                    }
                    fctype::PROPERTY_SET => {
                        self.handle_property_set_async(cid, sqe).await?;
                        Ok(DispatchOutcome::default())
                    }
                    fctype::DISCONNECT => {
                        self.handle_disconnect_async(cid, sqe).await?;
                        Ok(DispatchOutcome { disconnected: true })
                    }
                    other => {
                        tracing::warn!(
                            fctype = other,
                            "V8e-7-2 unsupported fctype; reply INVALID_FIELD"
                        );
                        self.send_capsule_resp_err_async(cid, 0x02).await?;
                        Ok(DispatchOutcome::default())
                    }
                }
            }
            crate::dispatch_plan::CapsuleKind::Admin { cid } => {
                // V8e-7-3 加完整 admin dispatch；V8e-7-2 占位用 INVALID_OPCODE
                tracing::debug!(cid, "V8e-7-2: admin cmd dispatch TODO V8e-7-3");
                self.send_capsule_resp_err_async(cid, 0x01).await?;
                Ok(DispatchOutcome::default())
            }
            crate::dispatch_plan::CapsuleKind::Io { cid } => {
                // V8e-7-3 加完整 IO dispatch
                tracing::debug!(cid, "V8e-7-2: IO cmd dispatch TODO V8e-7-3");
                self.send_capsule_resp_err_async(cid, 0x01).await?;
                Ok(DispatchOutcome::default())
            }
        }
    }

    /// **V8e-7-2** — 构造 `ConnStateSnapshot` 用于 `dispatch_plan` 决策。
    pub(crate) fn state_snapshot(&self) -> crate::dispatch_plan::ConnStateSnapshot {
        crate::dispatch_plan::ConnStateSnapshot {
            current_qid: self.current_qid,
            discovery_mode: self.discovery_mode,
            pending_aers_len: 0, // V8e-7-3 加 pending_aers 字段后真填
        }
    }

    /// **V8e-7-2** — async 版 fabric Connect 处理（与 sync `handle_connect`
    /// 1:1 等价）。Connect 成功 → 调 `set_kato` 注入 KATO timer。
    async fn handle_connect_async(
        &mut self,
        cid: u16,
        sqe: &[u8],
        data: &[u8],
    ) -> anyhow::Result<()> {
        if data.len() != fabric::CONNECT_DATA_SIZE {
            return self
                .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                .await;
        }
        let cd = match ConnectData::read_from_bytes(data) {
            Ok(c) => c,
            Err(_) => {
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
        };
        let fields = match fabric::decode_connect_fields(sqe) {
            Ok(f) => f,
            Err(_) => {
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
        };
        let qid = fields.qid;
        let kato = fields.kato;
        tracing::info!(
            qid,
            kato,
            subnqn = cd.subnqn_str(),
            hostnqn = cd.hostnqn_str(),
            discovery = self.discovery_mode,
            "V8e-7-2 Fabric Connect"
        );

        // **V7** discovery mode 校验 subnqn + reject IO queue
        if self.discovery_mode {
            if cd.subnqn_str() != crate::DISCOVERY_NQN {
                tracing::warn!("V7 Connect 拒：discovery mode 下 subnqn 必须 = DISCOVERY_NQN");
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
            if qid != 0 {
                tracing::warn!(qid, "V7 Connect 拒：discovery mode 下不支持 IO queue");
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
        }

        if qid == 0 {
            if self.admin_connected {
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
            self.admin_connected = true;
            self.current_qid = 0;
            // **V8e-5 / V8e-7-2** — KATO timer 在 admin Connect 成功后真 arm
            // （spec § 7.13；kato=0 disable）
            if kato > 0 {
                self.set_kato(kato);
            }
        } else {
            if !self.admin_connected {
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
            match self.io_queues.get_mut(&qid) {
                Some(state @ crate::io_queue::IoQueueState::Sq { .. }) => {
                    state.mark_connected();
                    self.current_qid = qid;
                }
                _ => {
                    tracing::warn!(qid, "V8e-7-2 Connect qid≥1 before Create IO SQ — reject");
                    return self
                        .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                        .await;
                }
            }
        }
        let cntlid = self.cntlid as u32;
        self.send_capsule_resp_ok_async(cid, cntlid).await
    }

    async fn handle_property_get_async(&mut self, cid: u16, sqe: &[u8]) -> anyhow::Result<()> {
        let pf = match fabric::decode_property_fields(sqe) {
            Ok(p) => p,
            Err(_) => return self.send_capsule_resp_err_async(cid, 0x02).await,
        };
        let size = match fabric::property_size_bytes(pf.attrib) {
            Ok(s) => s,
            Err(_) => return self.send_capsule_resp_err_async(cid, 0x02).await,
        };
        let ofst = pf.ofst;
        // V8b / V8e §3 Q1：parking_lot 短锁 closure 内不跨 await；
        // **V8e-7-2 R-4**：值先收完 → guard drop → 然后 await 写 wire
        let value_opt = {
            let mut c = self.controller.controller.lock();
            c.nvme_property_get(ofst, size)
        };
        let value = match value_opt {
            Some(v) => v,
            None => return self.send_capsule_resp_err_async(cid, 0x02).await,
        };
        let lo = (value & 0xFFFF_FFFF) as u32;
        let hi = (value >> 32) as u32;
        let hi_for_dw1 = if size == 8 { hi } else { 0 };
        self.send_capsule_resp_ok_with_dw1_async(cid, lo, hi_for_dw1)
            .await
    }

    async fn handle_property_set_async(&mut self, cid: u16, sqe: &[u8]) -> anyhow::Result<()> {
        let pf = match fabric::decode_property_fields(sqe) {
            Ok(p) => p,
            Err(_) => return self.send_capsule_resp_err_async(cid, 0x02).await,
        };
        let size = match fabric::property_size_bytes(pf.attrib) {
            Ok(s) => s,
            Err(_) => return self.send_capsule_resp_err_async(cid, 0x02).await,
        };
        let ofst = pf.ofst;
        let value = pf.value;
        let ok = {
            let mut t = pcie_vfio_user_sdk::NoopTransport;
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut t);
            self.controller
                .controller
                .lock()
                .nvme_property_set(&mut ctx, ofst, size, value)
        };
        if !ok {
            return self.send_capsule_resp_err_async(cid, 0x02).await;
        }
        self.send_capsule_resp_ok_async(cid, 0).await
    }

    /// **V8e-7-2** — async 版 fabric Disconnect (spec § 3.5)。
    /// 与 sync `handle_disconnect` 1:1 等价，但走 PumpEvent / DispatchOutcome
    /// 信号让 caller break（不 bail）。
    async fn handle_disconnect_async(&mut self, cid: u16, sqe: &[u8]) -> anyhow::Result<()> {
        if let Err(e) = fabric::decode_disconnect_fields(sqe) {
            tracing::warn!(error = %e, "V8e-7-2 Disconnect decode 失败");
            // SC=0x80 INVALID_CONNECT_FORMAT；之后 caller 收 outcome.disconnected
            // = true 让 conn break（与 sync H-1 fix 一致：reject 也 bail/close）
            self.send_capsule_resp_err_async(cid, 0x80).await?;
            return Ok(());
        }

        // V8b/V8d 同模式：用 session io_queues 镜像决定要拆的 qid（**不能**
        // nvme_list_io_sqs() 全拆会误删别 conn IO queue）
        let qids: Vec<u16> = self.io_queues.keys().copied().filter(|&q| q != 0).collect();
        let conn_id = self.conn_id;
        tracing::info!(
            conn_id,
            n_qids = qids.len(),
            qids = ?qids,
            "V8e-7-2 async Disconnect: deleting IO queues"
        );
        {
            let mut c = self.controller.controller.lock();
            for &q in &qids {
                let _ = c.nvme_delete_io_sq(q);
            }
            for &q in &qids {
                let _ = c.nvme_delete_io_cq(q);
            }
        }
        self.io_queues.clear();
        self.current_qid = 0;

        // ACK
        self.send_capsule_resp_ok_async(cid, 0).await
    }

    // -------- async wire emit helpers --------

    async fn send_capsule_resp_ok_async(
        &mut self,
        cid: u16,
        result_dw0: u32,
    ) -> anyhow::Result<()> {
        self.send_capsule_resp_ok_with_dw1_async(cid, result_dw0, 0)
            .await
    }

    async fn send_capsule_resp_ok_with_dw1_async(
        &mut self,
        cid: u16,
        result_dw0: u32,
        result_dw1: u32,
    ) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[0..4].copy_from_slice(&result_dw0.to_le_bytes());
        cqe[4..8].copy_from_slice(&result_dw1.to_le_bytes());
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu_async(&mut self.stream, &hdr, &cqe, &[])
            .await
            .context("V8e-7-2 async write CapsuleResp")
    }

    async fn send_capsule_resp_err_async(&mut self, cid: u16, sc: u8) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        let sct: u8 = if (0x80..=0x9F).contains(&sc) {
            0x07
        } else {
            0x00
        };
        let status: u16 = ((sct as u16) << 9) | ((sc as u16) << 1);
        cqe[14..16].copy_from_slice(&status.to_le_bytes());
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu_async(&mut self.stream, &hdr, &cqe, &[])
            .await
            .context("V8e-7-2 async write CapsuleResp err")
    }

    async fn send_c2h_term_async(&mut self, fes: u16) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_TERM,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let mut psh = [0u8; 16];
        psh[0..2].copy_from_slice(&fes.to_le_bytes());
        write_pdu_async(&mut self.stream, &hdr, &psh, &[])
            .await
            .context("V8e-7-2 async write C2HTermReq")
    }
}
