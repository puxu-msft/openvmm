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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream as TokioStream;
use tokio::time::{Instant as TokioInstant, Sleep, sleep_until};
use zerocopy::{FromBytes, IntoBytes};

/// **V-followup-tls-1** — `AsyncSession` 接受的 stream trait bound。
///
/// 当前 (V-followup-tls-1) 实测的实现：`tokio::net::TcpStream` 与 in-memory
/// `tokio::io::DuplexStream` 自动满足。V-followup-tls-3 引入
/// `tokio_rustls::server::TlsStream<TcpStream>` 后将自动满足；bin 端
/// `handle_conn_async` 届时根据是否 TLS 二分调用，由 monomorphize 各产一份代码。
///
/// `Send + 'static` 是 `tokio::spawn` 强制；`Unpin` 让我们能直接 `&mut self.stream`
/// 不需要 Pin projection。
pub trait AsyncSessionStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncSessionStream for T {}

/// **V8e-3** — async session：握手完成后通过 [`pump_one_async`] 接 PDU
/// 直到 shutdown 或 peer close。
///
/// 字段集合刻意收得比 sync `V2Session` 少：V8e-3 只交付 ICReq/Connect/简单
/// dispatch 骨架；V8e-4 加 AER Notify、V8e-5 加 KATO Sleep、V8e-6 加完整
/// admin/IO dispatch。
pub struct AsyncSession<S: AsyncSessionStream = TokioStream> {
    stream: S,
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

    // ============ V8e-7-3 新增：admin/IO/AER dispatch 所需 state ============
    /// **V8e-7-3** — TTAG 分配器（V4b R2T；admin/IO dma_read 用）。
    pub ttag_alloc: crate::TtagAllocator,
    /// **V8e-7-3** — session 镜像 pending AER 列表（cap MAX_PENDING_AERS=4）。
    /// controller `aen_pending` 是 source of truth；本字段仅做 cap + debug。
    pub pending_aers: Vec<crate::aer::PendingAer>,
    /// **V-followup-auth** — host NQN 白名单（None = 关闭白名单，等价 V8 行为；
    /// Some = 任何 Connect 的 `hostnqn` 必须在集合内，否则返
    /// `fabric_sc::CONNECT_INVALID_HOST` 关连接）。
    /// 由 `accept_and_handshake_async_with_auth` 或后续 setter 注入；
    /// `accept_and_handshake_async` 仍按 V8 行为不限制。
    pub host_nqn_allowlist: Option<std::sync::Arc<std::collections::HashSet<String>>>,
    /// **V-followup-auth-2** — TLS peer cert 抽出的 host identities（SAN URI /
    /// DNS / CN）。
    ///
    /// `None` = 不强制 NQN ↔ identity 绑定（plaintext / server-auth-only TLS /
    /// mTLS 但未启 binding）；`Some(set)` = Connect 的 `hostnqn` 必须 ∈ set，
    /// 否则返 `fabric_sc::CONNECT_INVALID_HOST` (0x84) 关 conn。
    ///
    /// 由 bin 端 mTLS handshake 完成后调 [`AsyncSession::bind_host_identities`]
    /// 注入；与 `host_nqn_allowlist` 是**且**关系（同时 enabled 时两关都过才能 Connect）。
    pub bound_host_identities: Option<std::collections::HashSet<String>>,
    /// **V-followup-dhchap-2** — CHAP secret store（共享 across all conn）。
    /// `None` = bin 未传 `--host-secret`（CHAP 完全 disabled）；`Some` = Connect
    /// 后会构造 `ChapNegotiation` 并把 stage 放入 [`Self::chap`]。
    pub chap_secret_store: Option<std::sync::Arc<crate::dhchap::ChapSecretStore>>,
    /// **V-followup-dhchap-2** — per-session CHAP 协商状态。Connect 入口由
    /// `chap_secret_store` 是否 Some 决定是否 init；非 None 时 admin/IO cmd 入口
    /// 必须 `stage.is_authenticated()` 才放行。
    pub chap: Option<crate::dhchap::ChapNegotiation>,
}

/// **V8e-3** — async 版 ICReq/ICResp handshake。语义与 sync
/// [`crate::ic_handshake`] 1:1 等价（HDGST/DDGST 协商写死禁用；MAXH2CDATA
/// = `MAXH2CDATA_BYTES`）。byte-stream 等价已被 V8e-1 regression gate 覆盖。
pub async fn ic_handshake_async<S>(stream: &mut S) -> anyhow::Result<crate::NegotiatedIc>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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
        // **V8e-7-followup byte-identical gate** — sync `ic_handshake` 在
        // session.rs:1399 处填 `MAXH2CDATA_BYTES`（64 KiB）；之前 V8e-3 这里
        // 写 0 是 bug（"让 host fallback" 的注释错误）— host 看 ICResp 时
        // maxh2cdata 字段是 spec 必填项，sync/async 两路径必须 byte-identical。
        maxr2t_or_maxh2cdata: crate::MAXH2CDATA_BYTES,
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
pub async fn accept_and_handshake_async<S>(
    mut stream: S,
    controller: SharedController,
) -> anyhow::Result<AsyncSession<S>>
where
    S: AsyncSessionStream,
{
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
        ttag_alloc: crate::TtagAllocator::default(),
        pending_aers: Vec::new(),
        host_nqn_allowlist: None,
        bound_host_identities: None,
        chap_secret_store: None,
        chap: None,
    })
}

/// **V-followup-auth** — `accept_and_handshake_async` 的带白名单变体。
///
/// 行为同 [`accept_and_handshake_async`]，但 session 内带 host NQN 白名单。
/// 后续 Connect 校验 `hostnqn` 必须在 set 内，否则返
/// `fabric_sc::CONNECT_INVALID_HOST` 关连接。
///
/// `allowlist` 为空 `HashSet` 等价于 "全拒"（教学版强警告）；调用者要么
/// 用 [`accept_and_handshake_async`] 保留旧行为，要么 set 非空。
pub async fn accept_and_handshake_async_with_auth<S>(
    stream: S,
    controller: SharedController,
    allowlist: std::sync::Arc<std::collections::HashSet<String>>,
) -> anyhow::Result<AsyncSession<S>>
where
    S: AsyncSessionStream,
{
    let mut sess = accept_and_handshake_async(stream, controller).await?;
    sess.host_nqn_allowlist = Some(allowlist);
    Ok(sess)
}

impl<S: AsyncSessionStream> AsyncSession<S> {
    /// **V-followup-auth-2** — 在 mTLS handshake 完成后注入 host identity 集合
    /// （SAN URI / DNS / CN），开启 NQN ↔ TLS identity binding 校验。
    ///
    /// 调用时机：bin 端拿 `TlsStream::get_ref().1.peer_certificates()` →
    /// `extract_host_identities(leaf)` → `sess.bind_host_identities(ids)`。
    ///
    /// 后续 Connect 内 `cd.hostnqn_str()` 必须 ∈ `ids`，否则返
    /// `fabric_sc::CONNECT_INVALID_HOST` (0x84)。
    pub fn bind_host_identities(&mut self, identities: std::collections::HashSet<String>) {
        self.bound_host_identities = Some(identities);
    }

    /// **V-followup-dhchap-2** — 在 handshake 后注入 CHAP secret store。
    /// 之后 Connect 内会按 store 是否含本 host NQN 决定走 `Authenticated` /
    /// `Disabled` / `ChallengeNeeded` 路径。
    pub fn enable_chap(&mut self, store: std::sync::Arc<crate::dhchap::ChapSecretStore>) {
        self.chap_secret_store = Some(store);
    }

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
impl<S: AsyncSessionStream> Drop for AsyncSession<S> {
    fn drop(&mut self) {
        if self.conn_id == 0 {
            return;
        }
        let conn_id = self.conn_id;
        let qids: Vec<u16> = self.io_queues.keys().copied().filter(|&q| q != 0).collect();
        // V8d M-3：拆两 catch_unwind 防 AER cleanup panic 阻塞 IO queue sweep
        // **V8e-7-4 reviewer L-3/L-4**：AER cleanup 不需要 qids；sweep 直接
        // move qids（无 clone）。Disconnect path 已 clear io_queues，所以
        // 多数路径 qids 已空（vec 仅初分配）；peer close 路径才有真 qids。
        let shared_aer = std::sync::Arc::clone(&self.controller);
        let aer_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            shared_aer.controller.lock().nvme_cleanup_conn_aers(conn_id)
        }));
        let shared_sweep = std::sync::Arc::clone(&self.controller);
        let sweep_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut c = shared_sweep.controller.lock();
            for &q in &qids {
                c.nvme_delete_io_sq(q);
            }
            for &q in &qids {
                c.nvme_delete_io_cq(q);
            }
            qids.len()
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

impl<S: AsyncSessionStream> AsyncSession<S> {
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
                    fctype::AUTH_RECV => {
                        // **V-followup-dhchap-3-wire** — host pulls challenge
                        self.handle_auth_recv_async(cid).await?;
                        Ok(DispatchOutcome::default())
                    }
                    fctype::AUTH_SEND => {
                        // **V-followup-dhchap-3-wire** — host submits HMAC response
                        self.handle_auth_send_async(cid, &pdu.data).await?;
                        Ok(DispatchOutcome::default())
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
                // **V-followup-dhchap-3-wire** — CHAP gate：启了 CHAP 且尚未通过
                // 时拒所有 admin cmd（CHAP exchange 本身走 Fabric path 不到这）
                if let Some(neg) = self.chap.as_ref()
                    && !neg.stage.is_authenticated()
                {
                    tracing::warn!(
                        stage = ?neg.stage,
                        "V-followup-dhchap-3-wire admin cmd 拒：CHAP 尚未通过"
                    );
                    self.send_capsule_resp_err_async(cid, 0x83).await?;
                    return Ok(DispatchOutcome::default());
                }
                let sqe = &pdu.psh[..64];
                self.handle_admin_cmd_async(cid, sqe).await?;
                Ok(DispatchOutcome::default())
            }
            crate::dispatch_plan::CapsuleKind::Io { cid } => {
                // **V-followup-dhchap-3-wire** — 同 admin gate
                if let Some(neg) = self.chap.as_ref()
                    && !neg.stage.is_authenticated()
                {
                    tracing::warn!(
                        stage = ?neg.stage,
                        "V-followup-dhchap-3-wire IO cmd 拒：CHAP 尚未通过"
                    );
                    self.send_capsule_resp_err_async(cid, 0x83).await?;
                    return Ok(DispatchOutcome::default());
                }
                let sqe = &pdu.psh[..64];
                self.handle_io_cmd_async(cid, sqe).await?;
                Ok(DispatchOutcome::default())
            }
        }
    }

    /// **V8e-7-2** — 构造 `ConnStateSnapshot` 用于 `dispatch_plan` 决策。
    pub(crate) fn state_snapshot(&self) -> crate::dispatch_plan::ConnStateSnapshot {
        crate::dispatch_plan::ConnStateSnapshot {
            current_qid: self.current_qid,
            discovery_mode: self.discovery_mode,
            pending_aers_len: self.pending_aers.len(),
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

        // **V-followup-auth** — host NQN 白名单校验（None=不限制；Some=必须在 set 内）
        if let Some(allow) = self.host_nqn_allowlist.as_ref() {
            let hostnqn = cd.hostnqn_str().to_string();
            if !allow.contains(&hostnqn) {
                tracing::warn!(
                    hostnqn = %hostnqn,
                    "V-followup-auth Connect 拒：hostnqn 不在 --allow-host-nqn 白名单"
                );
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_HOST)
                    .await;
            }
        }

        // **V-followup-auth-2** — NQN ↔ TLS cert identity binding 校验
        // （仅 mTLS 路径 bin 端注入后生效；plaintext / server-auth-only 不影响）
        if let Some(ids) = self.bound_host_identities.as_ref() {
            let hostnqn = cd.hostnqn_str().to_string();
            if !ids.contains(&hostnqn) {
                tracing::warn!(
                    hostnqn = %hostnqn,
                    bound_ids = ?ids,
                    "V-followup-auth-2 Connect 拒：hostnqn 不在 TLS cert SAN/CN 列表"
                );
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_HOST)
                    .await;
            }
        }

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
            // **V-followup-dhchap-2** — Connect 成功后启动 CHAP 协商（若 store 已注入）。
            // 注意：本 phase 不挂 AUTH wire dispatch；store 不为 None 时
            // `chap.stage` 进入 `ChallengeNeeded` 但 V-dhchap-3 才有真 wire 路径
            // 把它推进到 Authenticated。教学说明：用户启了 --host-secret 但没
            // 跑到 V-dhchap-3 时会卡在 ChallengeNeeded，本 phase 暂不 gate
            // admin/IO 命令（兼容性优先），只把 state 暴露出来供测试观察。
            if let Some(store) = self.chap_secret_store.as_ref() {
                self.chap = Some(crate::dhchap::ChapNegotiation::on_connect(
                    std::sync::Arc::clone(store),
                    cd.hostnqn_str(),
                    cd.subnqn_str(),
                ));
                tracing::info!(
                    stage = ?self.chap.as_ref().map(|c| &c.stage),
                    "V-followup-dhchap-2 CHAP 协商初始化"
                );
            }
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

    /// **V-followup-dhchap-3-wire** — 处理 AUTH_RECV：host 来拉 challenge。
    ///
    /// 教学版简化 wire：
    /// - host 发 capsule cmd fctype=AUTH_RECV，无 data
    /// - target 抽 challenge（推进 `ChallengeNeeded -> ChallengeSent`）
    /// - target 发 C2HData(challenge 32B) + CapsuleResp SC=0
    ///
    /// 状态非 `ChallengeNeeded` / 未启 CHAP / store 不含 host → SC=0x83
    /// （AUTHENTICATION_REQUIRED）关 conn-style 错误响应。
    async fn handle_auth_recv_async(&mut self, cid: u16) -> anyhow::Result<()> {
        let neg = match self.chap.as_mut() {
            Some(n) => n,
            None => {
                tracing::warn!("AUTH_RECV 拒：session 未 enable_chap");
                return self.send_capsule_resp_err_async(cid, 0x83).await;
            }
        };
        let challenge = match neg.issue_challenge() {
            Some(c) => c,
            None => {
                tracing::warn!(
                    stage = ?neg.stage,
                    "AUTH_RECV 拒：当前 state 不允许 issue_challenge"
                );
                return self.send_capsule_resp_err_async(cid, 0x83).await;
            }
        };
        // 教学版：把 challenge 整段（32B）作 C2HData payload 一次发完
        self.send_c2h_data_async(cid, &challenge).await?;
        self.send_capsule_resp_ok_async(cid, 0).await
    }

    /// **V-followup-dhchap-3-wire** — 处理 AUTH_SEND：host 提交 HMAC response。
    ///
    /// 教学版简化 wire：
    /// - host 发 capsule cmd fctype=AUTH_SEND，data 段 = HMAC-SHA256 response (32B)
    /// - target 调 `verify_host_response`；通过 → CapsuleResp SC=0，state ->
    ///   `Authenticated`；失败 → SC=0x83，state -> `Failed`
    ///
    /// data 段长度 ≠ 32B → SC=0x02 INVALID_FIELD。
    async fn handle_auth_send_async(&mut self, cid: u16, data: &[u8]) -> anyhow::Result<()> {
        let neg = match self.chap.as_mut() {
            Some(n) => n,
            None => {
                tracing::warn!("AUTH_SEND 拒：session 未 enable_chap");
                return self.send_capsule_resp_err_async(cid, 0x83).await;
            }
        };
        if data.len() != crate::dhchap::HMAC_SHA256_LEN {
            tracing::warn!(
                got = data.len(),
                expect = crate::dhchap::HMAC_SHA256_LEN,
                "AUTH_SEND 拒：response 长度非法"
            );
            return self.send_capsule_resp_err_async(cid, 0x02).await;
        }
        let mut resp = [0u8; crate::dhchap::HMAC_SHA256_LEN];
        resp.copy_from_slice(data);
        let ok = neg.verify_host_response(&resp);
        if ok {
            tracing::info!("V-followup-dhchap-3-wire CHAP 通过");
            self.send_capsule_resp_ok_async(cid, 0).await
        } else {
            tracing::warn!(
                stage = ?neg.stage,
                "V-followup-dhchap-3-wire CHAP 校验失败"
            );
            self.send_capsule_resp_err_async(cid, 0x83).await
        }
    }

    // ============ V8e-7-3 admin / IO async dispatch + AER drain ============

    /// **V8e-7-3** — async 版 admin cmd dispatch（与 sync `handle_admin_cmd`
    /// 1:1 等价）。
    async fn handle_admin_cmd_async(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        use crate::dispatch_plan::{
            AdminAerDecision, AdminBlockedOpcDecision, DiscoveryWhitelistDecision,
            decide_admin_aer_path, decide_admin_blocked_opc, decide_admin_discovery_whitelist,
        };
        use pcie_remote_nvme_userspace::cmd::Sqe;

        let mut sqe = Sqe::read_from_bytes(sqe_bytes)
            .map_err(|_| anyhow::anyhow!("V8e-7-3 admin SQE 不是 64 byte"))?;

        let state = self.state_snapshot();

        // V7 discovery 白名单
        match decide_admin_discovery_whitelist(sqe_bytes, state) {
            DiscoveryWhitelistDecision::Rejected => {
                tracing::warn!(
                    opc = crate::aer::peek_admin_opc(sqe_bytes),
                    "V8e-7-3 discovery 拒非白名单 opc"
                );
                return self.send_capsule_resp_err_async(cid, 0x01).await;
            }
            DiscoveryWhitelistDecision::Allowed | DiscoveryWhitelistDecision::NotDiscoveryMode => {}
        }

        // V6a AER fast-path
        match decide_admin_aer_path(sqe_bytes, state) {
            AdminAerDecision::OverCap => {
                tracing::warn!(cid, "V6a/V8e-7-3 AER over MAX_PENDING_AERS");
                return self.send_capsule_resp_err_async(cid, 0x05).await;
            }
            AdminAerDecision::FastPath => {
                return self.handle_admin_aer_fast_path_async(cid, sqe).await;
            }
            AdminAerDecision::NotAer => {}
        }

        // V5e-1-fix NS-shape 黑名单
        if matches!(
            decide_admin_blocked_opc(sqe_bytes),
            AdminBlockedOpcDecision::Blocked
        ) {
            tracing::warn!(
                opc = crate::aer::peek_admin_opc(sqe_bytes),
                "V8e-7-3 session 拒 NS-shape mutating opc"
            );
            return self.send_capsule_resp_err_async(cid, 0x01).await;
        }

        // PRP sentinel 改写
        sqe.prp1 = crate::PRP1_SENTINEL;
        sqe.prp2 = 0;
        let opc = (sqe.cdw0 & 0xff) as u8;

        // Create IO CQ/SQ peek（V5a）
        let create_io_cq_qid: Option<u16> = (opc == 0x05).then_some((sqe.cdw10 & 0xffff) as u16);
        let create_io_sq_pair: Option<(u16, u16)> = (opc == 0x01).then(|| {
            let sq_id = (sqe.cdw10 & 0xffff) as u16;
            let cq_id = ((sqe.cdw11 >> 16) & 0xffff) as u16;
            (sq_id, cq_id)
        });
        if let Some(qid) = create_io_cq_qid {
            sqe.prp1 = crate::session::cq_sentinel(qid);
        }

        // dispatch in short lock
        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        let conn_id = self.conn_id;
        let immediate_cqe = {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            c.nvme_admin_dispatch_with_conn(&mut ctx, sqe, cid, 0, conn_id)
        };

        // V5a：Create IO CQ/SQ success 后镜像
        if let Some(cqe) = immediate_cqe.as_ref() {
            let dw3 = cqe.dw3;
            let sc = ((dw3 >> 17) & 0xff) as u8;
            if sc == 0 {
                if let Some(qid) = create_io_cq_qid {
                    let sentinel = crate::session::cq_sentinel(qid);
                    self.io_queues
                        .insert(qid, crate::io_queue::IoQueueState::new_cq(sentinel));
                }
                if let Some((sq_id, cq_id)) = create_io_sq_pair {
                    self.io_queues
                        .insert(sq_id, crate::io_queue::IoQueueState::new_sq(cq_id));
                }
            }
        }

        self.run_post_dispatch_async(cid, immediate_cqe, tcp_t)
            .await
    }

    /// **V8e-7-3** — AER fast-path：dispatch 让 controller `aen_pending`
    /// push；session 镜像 push；跳过 run_post_dispatch；wire 上不 emit
    /// CapsuleResp（等 drain_aers_async fire 时真发）。
    async fn handle_admin_aer_fast_path_async(
        &mut self,
        cid: u16,
        mut sqe: pcie_remote_nvme_userspace::cmd::Sqe,
    ) -> anyhow::Result<()> {
        sqe.prp1 = crate::PRP1_SENTINEL;
        sqe.prp2 = 0;
        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        let conn_id = self.conn_id;
        let immediate = {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            c.nvme_admin_dispatch_with_conn(&mut ctx, sqe, cid, 0, conn_id)
        };
        self.next_token = tcp_t.token_high_water();
        if immediate.is_some() {
            anyhow::bail!(
                "V8e-7-3 AER invariant: dispatch returned Some(Cqe), spec § 5.2 must stash"
            );
        }
        if !tcp_t.writes.is_empty() || !tcp_t.pending_reads.is_empty() {
            anyhow::bail!(
                "V8e-7-3 AER invariant: dispatch produced DMA writes/reads ({}/{})",
                tcp_t.writes.len(),
                tcp_t.pending_reads.len()
            );
        }
        self.pending_aers.push(crate::aer::PendingAer {
            cid,
            sq_id: 0,
            registered_at: std::time::Instant::now(),
        });
        tracing::debug!(cid, "V8e-7-3 AER stashed");
        Ok(())
    }

    /// **V8e-7-3** — async 版 IO cmd dispatch（与 sync `handle_io_cmd` 1:1）。
    async fn handle_io_cmd_async(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        use crate::dispatch_plan::{IoNlbDecision, decide_io_nlb_check, prp2_sentinel_for_nlb};
        use pcie_remote_nvme_userspace::cmd::Sqe;

        let mut sqe = Sqe::read_from_bytes(sqe_bytes)
            .map_err(|_| anyhow::anyhow!("V8e-7-3 IO SQE 不是 64 byte"))?;

        // V5b R-5：清 PSDT bits
        sqe.cdw0 &= !(0b11u32 << 14);
        sqe.prp1 = crate::PRP1_SENTINEL;

        let sq_id = self.current_qid;
        let cq_id = match self.io_queues.get(&sq_id) {
            Some(crate::io_queue::IoQueueState::Sq { cq_id, .. }) => *cq_id,
            _ => anyhow::bail!(
                "V8e-7-3 invariant: handle_io_cmd_async 但 current_qid={sq_id} 不是 IO SQ"
            ),
        };

        // NLB + dual-PRP sentinel 决策
        match decide_io_nlb_check(&sqe) {
            IoNlbDecision::OverMax { nlb_real } => {
                tracing::warn!(opc = (sqe.cdw0 & 0xff) as u8, nlb_real, "V5e-2 nlb>MAX");
                return self.send_capsule_resp_err_async(cid, 0x18).await;
            }
            IoNlbDecision::Ok { nlb_real } => {
                sqe.prp2 = prp2_sentinel_for_nlb(nlb_real);
            }
            IoNlbDecision::NotReadWrite => {
                sqe.prp2 = 0;
            }
        }

        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        let immediate_cqe = {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            c.nvme_io_dispatch(&mut ctx, sq_id, sqe, cid, cq_id)
        };
        self.run_post_dispatch_async(cid, immediate_cqe, tcp_t)
            .await
    }

    /// **V8e-7-3** — async run_post_dispatch（与 sync `run_post_dispatch`
    /// 1:1 等价；R2T loop 走 V8e §3 Q5 三段式：lock-pop / unlock-await-wire /
    /// lock-complete）。
    async fn run_post_dispatch_async(
        &mut self,
        cid: u16,
        immediate_cqe: Option<pcie_remote_nvme_userspace::cmd::Cqe>,
        mut tcp_t: crate::tcp_transport::TcpAdminTransport,
    ) -> anyhow::Result<()> {
        // Phase 1.5: mixed-path guard
        let dispatch_data_writes = tcp_t
            .writes
            .iter()
            .filter(|w| w.gpa < crate::CQ_BASE_GPA)
            .count();
        let dispatch_pending_reads = tcp_t.pending_reads.len();
        if dispatch_data_writes > 0 && dispatch_pending_reads > 0 {
            anyhow::bail!(
                "V4b/V5 invariant violation: cmd produced both data_write ({}) and dma_read ({})",
                dispatch_data_writes,
                dispatch_pending_reads
            );
        }
        let had_pending_reads = dispatch_pending_reads > 0;

        // Phase 2: dma_read 闭环走 R2T 三段式
        let mut cmd_cumulative_offset: u32 = 0;
        while let Some(read_req) = tcp_t.pop_read() {
            tracing::debug!(
                cid,
                token = read_req.token,
                len = read_req.len,
                cumulative_offset = cmd_cumulative_offset,
                "V8e-7-3 async dma_read R2T 三段式"
            );
            // 第 2 段：unlock-await-wire（dma_read_via_r2t_async 内部 read_pdu_async）
            let bytes = self
                .dma_read_via_r2t_async(cid, cmd_cumulative_offset, read_req.len)
                .await
                .with_context(|| {
                    format!(
                        "V8e-7-3 dma_read failed (cid={cid}, token={tok}, len={l}, offset={off})",
                        tok = read_req.token,
                        l = read_req.len,
                        off = cmd_cumulative_offset,
                    )
                })?;
            cmd_cumulative_offset = cmd_cumulative_offset.saturating_add(read_req.len);
            // 第 3 段：lock-complete
            {
                let mut c = self.controller.controller.lock();
                let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
                c.nvme_admin_complete_dma(&mut ctx, read_req.token, true, bytes);
            }
        }

        // Phase 3: 同步 / 异步 write-out
        if let Some(cqe) = immediate_cqe {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            c.nvme_post_cqe(&mut ctx, cqe);
        } else if !had_pending_reads {
            let mut data_tokens = Vec::with_capacity(tcp_t.writes.len());
            for w in tcp_t.writes.iter() {
                if w.gpa >= crate::CQ_BASE_GPA {
                    anyhow::bail!(
                        "V3 invariant violation: async dispatch produced CQE write \
                         before on_dma_complete (gpa={:#x})",
                        { w.gpa }
                    );
                }
                data_tokens.push(w.token);
            }
            for tok in data_tokens {
                let mut c = self.controller.controller.lock();
                let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
                c.nvme_admin_complete_dma(&mut ctx, tok, true, Vec::new());
            }
        }

        // 保存 token high water
        self.next_token = tcp_t.token_high_water();

        // Phase 4: drain captured.writes → data + cqe（与 sync 完全一致）
        let mut data_payload = Vec::new();
        let mut cqe_bytes: Option<Vec<u8>> = None;
        while let Some(w) = tcp_t.pop_write() {
            if w.gpa >= crate::CQ_BASE_GPA {
                if cqe_bytes.is_some() {
                    anyhow::bail!("V3 invariant violation: multiple CQE writes for single cmd");
                }
                if w.data.len() != 16 {
                    anyhow::bail!("captured CQE write len={} != 16", w.data.len());
                }
                cqe_bytes = Some(w.data);
            } else {
                if cqe_bytes.is_some() {
                    anyhow::bail!(
                        "V3 invariant violation: data write after CQE write (gpa={:#x}, {}B)",
                        { w.gpa },
                        w.data.len()
                    );
                }
                data_payload.extend_from_slice(&w.data);
            }
        }
        let cqe_bytes = cqe_bytes
            .ok_or_else(|| anyhow::anyhow!("V8e-7-3: controller did not produce CQE for cmd"))?;

        // Phase 5: emit C2HData + CapsuleResp
        if !data_payload.is_empty() {
            self.send_c2h_data_async(cid, &data_payload).await?;
        }
        self.write_capsule_resp_bytes_async(&cqe_bytes).await
    }

    /// **V8e-7-3** — async 版 dma_read_via_r2t（V4c 多 R2T 串行；V5e-2
    /// base_offset 累计）。
    async fn dma_read_via_r2t_async(
        &mut self,
        cid: u16,
        base_offset: u32,
        len: u32,
    ) -> anyhow::Result<Vec<u8>> {
        if len > crate::session::V4_MAX_DMA_READ_BYTES {
            anyhow::bail!(
                "V8e-7-3 dma_read len {len} exceeds cap {}",
                crate::session::V4_MAX_DMA_READ_BYTES
            );
        }
        let max = crate::MAXH2CDATA_BYTES;
        let mut buf: Vec<u8> = Vec::with_capacity(len as usize);
        let mut offset_in_this_read: u32 = 0;
        while offset_in_this_read < len {
            let remaining = len - offset_in_this_read;
            let chunk = remaining.min(max);
            let cmd_offset = base_offset.saturating_add(offset_in_this_read);
            let bytes = self
                .dma_read_one_chunk_async(cid, cmd_offset, chunk)
                .await?;
            buf.extend_from_slice(&bytes);
            offset_in_this_read += chunk;
        }
        debug_assert_eq!(buf.len(), len as usize);
        Ok(buf)
    }

    /// **V8e-7-3** — 单片 R2T 子路径 (async)。
    async fn dma_read_one_chunk_async(
        &mut self,
        cid: u16,
        offset: u32,
        chunk: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let ttag = self.ttag_alloc.alloc();
        let (hdr, psh) = crate::r2t::encode_r2t(cid, ttag, offset, chunk);
        write_pdu_async(&mut self.stream, &hdr, psh.as_bytes(), &[])
            .await
            .context("V8e-7-3 async write R2T PDU")?;
        await_host_data_async(&mut self.stream, cid, ttag, offset, chunk)
            .await
            .with_context(|| {
                format!("V8e-7-3 await_host_data failed (ttag={ttag}, off={offset}, chunk={chunk})")
            })
    }

    /// **V8e-7-3** — 发 C2HData PDU（一次性 + DATA_LAST）。
    async fn send_c2h_data_async(&mut self, cid: u16, data: &[u8]) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags: crate::pdu::flags::DATA_LAST,
            hlen: 24,
            pdo: 24,
            plen: 24 + data.len() as u32,
        };
        let psh = crate::pdu::DataPsh {
            cccid: cid,
            ttag_or_rsvd: 0,
            data_offset: 0,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        write_pdu_async(&mut self.stream, &hdr, psh.as_bytes(), data)
            .await
            .context("V8e-7-3 async write C2HData")
    }

    /// **V8e-7-3** — 把 controller 已 dma_write 的 16-byte CQE bytes 当
    /// CapsuleResp PSH 直接 emit（spec：RSP PDU PSH = CQE）。
    async fn write_capsule_resp_bytes_async(&mut self, cqe_bytes: &[u8]) -> anyhow::Result<()> {
        debug_assert_eq!(cqe_bytes.len(), 16);
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu_async(&mut self.stream, &hdr, cqe_bytes, &[])
            .await
            .context("V8e-7-3 async write CapsuleResp raw CQE")
    }

    /// **V8e-7-3** — caller (handle_conn_async) 收到 `PumpEvent::AenReady`
    /// 后调，把 controller 端 pending AER fire + capture CQE → wire CapsuleResp。
    ///
    /// 一次最多 drain `MAX_DRAIN_PER_PUMP=4` 条（与 V6b sync `pump_one_with_events`
    /// fairness 等价）。返实际 emit 条数（0 = spurious wakeup / 无 pending）。
    pub async fn drain_aers_async(&mut self) -> anyhow::Result<usize> {
        const MAX_DRAIN_PER_PUMP: usize = 4;
        let conn_id = self.conn_id;
        let mut emitted = 0usize;
        for _ in 0..MAX_DRAIN_PER_PUMP {
            let mut tcp_t =
                crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
            let fired = {
                let mut c = self.controller.controller.lock();
                if c.nvme_pending_aer_count_for_conn(conn_id) == 0 {
                    false
                } else {
                    let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
                    c.nvme_fire_aen_for_conn(
                        &mut ctx, /*type*/ 0, /*info*/ 0, /*log_id*/ 0, conn_id,
                    )
                }
            };
            self.next_token = tcp_t.token_high_water();
            if !fired {
                break;
            }
            // captured 应只含 1 条 CQE write（fire_aen 不产 data write）
            let mut cqe_bytes: Option<Vec<u8>> = None;
            while let Some(w) = tcp_t.pop_write() {
                if w.gpa < crate::CQ_BASE_GPA {
                    anyhow::bail!(
                        "V8e-7-3 AER drain invariant: unexpected data write (gpa={:#x}, {}B)",
                        { w.gpa },
                        w.data.len()
                    );
                }
                if cqe_bytes.is_some() {
                    anyhow::bail!("V8e-7-3 AER drain invariant: multiple CQE writes per fire");
                }
                if w.data.len() != 16 {
                    anyhow::bail!("V8e-7-3 AER drain captured CQE len={} != 16", w.data.len());
                }
                cqe_bytes = Some(w.data);
            }
            let Some(cqe_bytes) = cqe_bytes else {
                break;
            };
            self.write_capsule_resp_bytes_async(&cqe_bytes).await?;
            // session 镜像 pending_aers 同步 -1（front-pop = FIFO）
            if !self.pending_aers.is_empty() {
                self.pending_aers.remove(0);
            }
            emitted += 1;
        }
        tracing::debug!(
            conn_id,
            emitted,
            mirror = self.pending_aers.len(),
            "V8e-7-3 drain_aers_async"
        );
        Ok(emitted)
    }

    /// **V8e-7-3 test-only** — 让测试 / V8e-followup 在 session 上下文内
    /// 触发 controller fire_aen + drain wire。与 V6b sync `inject_aen` 1:1
    /// 等价但走 async path。
    ///
    /// **V8e-7-4 reviewer M-2** — 本 API 不维护 `pending_aers` 镜像与 caller
    /// stash 顺序的一致性（仅 `if !is_empty { remove(0) }` 兜底）；仅供
    /// test 用。prod path 走 `dispatch_pdu_async` AER fast-path 让
    /// `pending_aers` push → caller 收 `PumpEvent::AenReady` 后调
    /// `drain_aers_async` 闭环。
    ///
    /// **V8e-7 security-reviewer MEDIUM-1** — 加 `#[doc(hidden)]` +
    /// `#[deprecated]` 让任何 prod caller 调用产 warning；测试 + V-followup
    /// 显式标注 `#[allow(deprecated)]` 即可。
    #[doc(hidden)]
    #[deprecated(
        note = "test-only; prod path 走 dispatch_pdu_async AER fast-path → drain_aers_async"
    )]
    pub async fn inject_aen_async(
        &mut self,
        aen_type: u8,
        aen_info: u8,
        log_id: u8,
    ) -> anyhow::Result<usize> {
        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        let conn_id = self.conn_id;
        let fired = {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_remote_userspace_sdk::DeviceCtx::new(&mut tcp_t);
            c.nvme_fire_aen_for_conn(&mut ctx, aen_type, aen_info, log_id, conn_id)
        };
        self.next_token = tcp_t.token_high_water();
        if !fired {
            return Ok(0);
        }
        // 1 条 CQE
        let mut emitted = 0usize;
        while let Some(w) = tcp_t.pop_write() {
            if w.gpa >= crate::CQ_BASE_GPA && w.data.len() == 16 {
                self.write_capsule_resp_bytes_async(&w.data).await?;
                emitted += 1;
            }
        }
        if !self.pending_aers.is_empty() {
            self.pending_aers.remove(0);
        }
        Ok(emitted)
    }
}

/// **V8e-7-3** — async 版 await_host_data（与 sync 等价）。
async fn await_host_data_async<S>(
    stream: &mut S,
    cid: u16,
    ttag: u16,
    base_offset: u32,
    expected_len: u32,
) -> anyhow::Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut r = crate::H2cReassembler::with_base_offset(cid, ttag, base_offset, expected_len);
    loop {
        let pdu = read_pdu_async(stream)
            .await
            .context("V8e-7-3 read H2CData")?;
        let pt = pdu.header.pdu_type;
        if pt == pdu_type::H2C_TERM {
            anyhow::bail!("V8e-7-3 host sent H2CTermReq while awaiting H2CData");
        }
        match r.accept_pdu(&pdu) {
            crate::AcceptOutcome::Continue => continue,
            crate::AcceptOutcome::Done(bytes) => return Ok(bytes),
            crate::AcceptOutcome::Error { fes, reason } => {
                // emit C2HTerm
                let term_hdr = CommonHdr {
                    pdu_type: pdu_type::C2H_TERM,
                    flags: 0,
                    hlen: 24,
                    pdo: 0,
                    plen: 24,
                };
                let mut term_psh = [0u8; 16];
                term_psh[0..2].copy_from_slice(&fes.to_le_bytes());
                let _ = write_pdu_async(stream, &term_hdr, &term_psh, &[]).await;
                anyhow::bail!("V8e-7-3 H2CData reassembly: fes={fes:#x} reason={reason}");
            }
        }
    }
}
