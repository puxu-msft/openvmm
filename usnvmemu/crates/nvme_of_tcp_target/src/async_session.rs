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
/// `Send + 'static` 是 `tokio::spawn` 强制；`Unpin` 让我们能直接 `&mut self.backend.stream`
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
    /// **V9 R2** — TCP 出站数据移动后端（owns stream + ttag 分配 + MAXH2CDATA）。
    /// pump 的 recv `select!` 借 `backend.stream` 字段；数据移动经 `FabricBackend` trait。
    pub(crate) backend: crate::fabric_backend::TcpFabricBackend<S>,
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
    /// **2026-06-09 纯 4K** — 是否放行 host 的 Format NVM (opc 0x80)。默认
    /// `false`（保守 block 防误改 NS 形状）；bin 经 CLI `--allow-format` 显式
    /// opt-in。session 已扇区感知，Format 改 lbads 后下条 IO 即按新扇区合成
    /// PRP。0x0D NS Management 不受此开关影响，恒 block。
    pub allow_format: bool,
    /// **2026-06-09 fused C&W over fabric** — 暂存的 fused FIRST(Compare) 的
    /// SQE + host cccid，等其 SECOND(Write) 到达配对。session **独占**此 fuse
    /// 状态（不依赖 controller `pending_fused`）：两条 FIRST / fuse=0 打断 /
    /// SECOND-without-FIRST / nsid·slba·nlb 不匹配 全在 session 内一致处理，
    /// 不会出现 session 与 controller 双状态失步（reviewer HIGH-1）。controller
    /// `nvme_fused_cas` 是无状态原子 CAS（host 两 buffer 经 R2T 取齐后单次
    /// `&mut self` 调用内 read-compare-write，不中途释放锁 → reviewer HIGH-2）。
    pub pending_fused: Option<(nvme_firmware::cmd::Sqe, u16)>,
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
    let h_type = pdu.header.pdu_type;
    let h_hlen = pdu.header.hlen;
    let h_plen = pdu.header.plen;
    if h_type != pdu_type::ICREQ {
        anyhow::bail!(
            "V8e-3: expected ICReq (0x00), got pdu_type={:#x} hlen={} plen={}",
            h_type,
            h_hlen,
            h_plen
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
        backend: crate::fabric_backend::TcpFabricBackend::new(stream, crate::MAXH2CDATA_BYTES),
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
        pending_aers: Vec::new(),
        host_nqn_allowlist: None,
        bound_host_identities: None,
        chap_secret_store: None,
        chap: None,
        allow_format: false,
        pending_fused: None,
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

    /// **2026-06-09 纯 4K** — opt-in 放行 host Format NVM (opc 0x80)。bin 端按
    /// CLI `--allow-format` 调用。默认 false 时 Format 仍被 block。
    pub fn set_allow_format(&mut self, allow: bool) {
        self.allow_format = allow;
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
            // **R3 落地条件**（architect review）：RDMA recv = poll_cq（非 read stream），此处直借
            // `self.backend.stream` 是 TCP-specific 泄漏；R3 须抽 `FabricBackend::recv_next` 方法
            // 让本 arm 变 `self.backend.recv_next()`，TCP/RDMA 各内部实现。见 fabric_backend trait doc。
            r = read_pdu_async(&mut self.backend.stream) => {
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
        // **transport-SGL Connect data（2026-06-14 真 WS2025 互通）** — Connect data（1024B）
        // 可 in-capsule（Linux/admin）或经 transport SGL 经 R2T 到达（Windows IO Connect）。
        let connect_data: Vec<u8> = match fabric::decode_connect_data_source(sqe, data.len()) {
            fabric::ConnectDataSource::InCapsule => data.to_vec(),
            fabric::ConnectDataSource::ViaR2t { len } => {
                // Windows IO Connect：发 R2T 取 Connect data。host 半开/卡死的挂起由
                // `await_host_data_async` 内的 `H2C_DATA_READ_TIMEOUT` 兜底（覆盖全 R2T 路径）。
                self.dma_read_via_r2t_async(cid, 0, len)
                    .await
                    .context("Connect data via R2T (transport SGL)")?
            }
            fabric::ConnectDataSource::Invalid => {
                return self
                    .send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM)
                    .await;
            }
        };
        let cd = match ConnectData::read_from_bytes(connect_data.as_slice()) {
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

        // **static controller model（2026-06-14）** — 校验 host 请求的 CNTLID
        // （spec § 3.3）。单 controller target：dynamic(0xFFFF)/static-any(0xFFFE)/具体
        // 匹配(==our) 都 accept → 分配 self.cntlid；指定一个我们没有的具体 cntlid →
        // reject SC=0x82 + IPO/IATTR 指向 data 内 cntlid 字段。修「静默 coerce 成 1」缺口。
        let requested_cntlid = cd.cntlid;
        if matches!(
            crate::dispatch_plan::decide_connect_cntlid(requested_cntlid, self.cntlid),
            crate::dispatch_plan::ConnectCntlidDecision::RejectInvalidParam
        ) {
            tracing::warn!(
                requested_cntlid,
                our_cntlid = self.cntlid,
                "static controller model Connect 拒：请求的具体 CNTLID 不存在"
            );
            return self
                .send_capsule_resp_err_with_result_async(
                    cid,
                    fabric_sc::CONNECT_INVALID_PARAM,
                    fabric::CONNECT_CNTLID_INVALID_RESULT_DW0,
                )
                .await;
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
            // **V-followup-interop-2** — NVMe-oF fabric IO queue Connect。
            // 与 PCIe 路径区别：fabric host **不** 发 `Create IO CQ`+`Create
            // IO SQ` 双 admin cmd（那是 PCIe-only）；spec § 3.6 fabric IO
            // queue 由 Connect (qid≥1) 直接创建。每条 IO queue = 独立 TCP
            // 连接 + 独立 AsyncSession，所以 self.admin_connected 在 IO 端永
            // 远 false（admin queue 是另一条 TCP）。
            //
            // 行为：
            // 1. 不校验 self.admin_connected（per-session 字段对 fabric IO 无意义）
            // 2. 强制 install controller-side IO CQ + SQ (force_install 容忍
            //    重 Connect / overwrite)
            // 3. session 端 io_queues 镜像加 Sq{connected=true}，让 IO cmd
            //    dispatch_plan::CapsuleKind::Io 路径生效
            let cq_sentinel = crate::session::cq_sentinel(qid);
            let qsize = fields.sqsize as u32 + 1; // sqsize 是 0-based
            {
                let mut c = self.controller.controller.lock();
                c.nvme_force_install_io_queue(qid, cq_sentinel, qsize);
            }
            self.io_queues
                .insert(qid, crate::io_queue::IoQueueState::new_cq(cq_sentinel));
            // 立即 transit 到 Sq{connected=true}：fabric 把 CQ+SQ install 合一
            self.io_queues
                .insert(qid, crate::io_queue::IoQueueState::new_sq(qid));
            if let Some(state) = self.io_queues.get_mut(&qid) {
                state.mark_connected();
            }
            self.current_qid = qid;
            tracing::info!(
                qid,
                qsize,
                cq_sentinel = format_args!("{:#x}", cq_sentinel),
                "V-followup-interop-2 fabric IO queue auto-installed via Connect"
            );
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
            let mut t = pcie_device_core::NoopTransport;
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut t);
            self.controller
                .controller
                .lock()
                .nvme_property_set(&mut ctx, ofst, size, value)
        };
        if !ok {
            return self.send_capsule_resp_err_async(cid, 0x02).await;
        }
        // **V-followup-interop-1 修复** — Linux nvme-tcp host 走 fabric path
        // 不会写 ASQ/ACQ register (NVMe-oF spec § 3.6 admin queue 由 Fabric
        // Connect 建立)，所以 CC.EN 0→1 触发的 `controller.enable()` 会把
        // `cqs[0].base_gpa` 重置为 `self.acq = 0` (默认值)，导致 admin CQE
        // 被 post 到 gpa=0 而非我们的 sentinel CQ_BASE_GPA → dispatch 拿不到
        // CQE → "controller did not produce CQE for cmd" Err。
        //
        // 修复: 见 CC 写 + EN bit 时重 install admin CQ 到 CQ_BASE_GPA。
        // CC 寄存器在 fabric 上仍可正常 read/write 给 host 做 polling，但
        // admin CQ 实际 sentinel 永远是我们这个。
        if ofst == fabric::property_offset::CC && (value as u32) & 0x1 != 0 {
            let mut c = self.controller.controller.lock();
            // **V-followup-interop-1** — `enable()` 用 `self.acq=0` 重置了
            // cqs[0]，普通 `nvme_install_admin_cq` 见已存在 cq[0] (base=0)
            // 会返 MismatchedParams 不替换。这里必须用 force_install 覆盖。
            c.nvme_force_install_admin_cq(crate::CQ_BASE_GPA, crate::ADMIN_CQ_SIZE);
            tracing::debug!(
                "V-followup-interop-1 CC.EN=1 后 force-reinstall admin CQ at CQ_BASE_GPA"
            );
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
        use crate::fabric_backend::FabricBackend as _;
        self.backend.send_response_capsule(&cqe).await
    }

    async fn send_capsule_resp_err_async(&mut self, cid: u16, sc: u8) -> anyhow::Result<()> {
        self.send_capsule_resp_err_with_result_async(cid, sc, 0)
            .await
    }

    /// 同 [`send_capsule_resp_err_async`] 但填 CQE result DW0（用于 Connect 失败
    /// 的 IPO/IATTR，见 `fabric::CONNECT_CNTLID_INVALID_RESULT_DW0`）。
    async fn send_capsule_resp_err_with_result_async(
        &mut self,
        cid: u16,
        sc: u8,
        result_dw0: u32,
    ) -> anyhow::Result<()> {
        let mut cqe = [0u8; 16];
        cqe[0..4].copy_from_slice(&result_dw0.to_le_bytes());
        cqe[12..14].copy_from_slice(&cid.to_le_bytes());
        // **SCT 修复（2026-06-14）** — NVMe-oF fabrics 命令状态码（0x80..=0x9F，
        // 含 Connect 0x80-0x84）属 **SCT=0x01 Command Specific Status**，不是
        // 0x07 Vendor Specific（原值是既存 bug：注释把 0x07 误标 Command Specific）。
        // IPO/IATTR 的 Dword0 语义只在 SCT=0x01 下定义；用 0x07 会让 conformant host
        // 把 0x82 当厂商私有错误，IPO/IATTR 失效。
        let sct: u8 = if (0x80..=0x9F).contains(&sc) {
            0x01
        } else {
            0x00
        };
        let status: u16 = ((sct as u16) << 9) | ((sc as u16) << 1);
        cqe[14..16].copy_from_slice(&status.to_le_bytes());
        use crate::fabric_backend::FabricBackend as _;
        self.backend.send_response_capsule(&cqe).await
    }

    async fn send_c2h_term_async(&mut self, fes: u16) -> anyhow::Result<()> {
        self.backend.send_c2h_term(fes).await
    }

    /// **V-followup-dhchap-3-wire / V-followup-dhchap-4** — 处理 AUTH_RECV：
    /// host 来拉 challenge / success1。
    ///
    /// 两种 wire 自动路由（基于 `chap.wire_mode`，由首个 AUTH_SEND 锁定）：
    ///
    /// **Simplified wire** (V-followup-dhchap-3 + Python tests 路径):
    /// - host 发 capsule cmd fctype=AUTH_RECV，无 data
    /// - target 抽 challenge（推进 `ChallengeNeeded -> ChallengeSent`）
    /// - target 发 C2HData(challenge 32B) + CapsuleResp SC=0
    ///
    /// **Spec § 8.13.5 4-msg wire** (Linux nvme-cli 路径):
    /// - 第一次 AUTH_RECV (state == `ChallengeNeeded`): 调 issue_challenge →
    ///   `ChallengeSent`, 回 CHALLENGE wire 包 (16B header + 32B cval), 内含
    ///   t_id / hashid=SHA-256 / dhgid=NULL
    /// - 第二次 AUTH_RECV (state == `Authenticated`): 回 SUCCESS1 wire 包
    ///   (16B, rvalid=0 unidirectional)
    ///
    /// reviewer H-1/M-5 修：Spec 路径所有非 OK 分支都额外发 FAILURE1 wire
    /// (16B) C2HData，让 Linux nvme-cli 拿到 rescode_exp 诊断信息。若
    /// `spec_tid == 0` (host 未跑 NEGOTIATE)，FAILURE1 t_id 也为 0，host 会
    /// 报 mismatch 但能识别失败。
    async fn handle_auth_recv_async(&mut self, cid: u16) -> anyhow::Result<()> {
        use crate::dhchap::{ChapStage, ChapWireMode};

        let neg = match self.chap.as_mut() {
            Some(n) => n,
            None => {
                tracing::warn!("AUTH_RECV 拒：session 未 enable_chap");
                return self.send_capsule_resp_err_async(cid, 0x83).await;
            }
        };

        // V-followup-dhchap-4: spec 4-msg 路径
        if neg.wire_mode == ChapWireMode::Spec4Msg {
            // 当前 stage 决定回 CHALLENGE 还是 SUCCESS1
            match &neg.stage {
                ChapStage::ChallengeNeeded { .. } => {
                    let tid = neg.spec_tid;
                    let challenge = match neg.issue_challenge() {
                        Some(c) => c,
                        None => {
                            tracing::warn!("AUTH_RECV (spec): issue_challenge 失败");
                            let fw = crate::dhchap::build_failure(
                                tid,
                                true,
                                crate::dhchap::wire::FAIL_EXP_FAILED,
                            );
                            self.send_c2h_data_async(cid, &fw).await?;
                            return self.send_capsule_resp_err_async(cid, 0x83).await;
                        }
                    };
                    let wire = crate::dhchap::build_challenge(tid, &challenge);
                    self.send_c2h_data_async(cid, &wire).await?;
                    return self.send_capsule_resp_ok_async(cid, 0).await;
                }
                ChapStage::Authenticated => {
                    // host 验过 REPLY 后第二次 AUTH_RECV 拉 SUCCESS1。
                    // 注：spec 双向 auth 时 SUCCESS1 携带 host-verify rval；本实现
                    // 仅 unidirectional，hl=32 但 rvalid=0 (Linux kernel 同此)。
                    let wire = crate::dhchap::build_success1(neg.spec_tid);
                    self.send_c2h_data_async(cid, &wire).await?;
                    return self.send_capsule_resp_ok_async(cid, 0).await;
                }
                other => {
                    tracing::warn!(stage = ?other, "AUTH_RECV (spec): state 不允许");
                    let fw = crate::dhchap::build_failure(
                        neg.spec_tid,
                        true,
                        crate::dhchap::wire::FAIL_EXP_INCORRECT_MESSAGE,
                    );
                    self.send_c2h_data_async(cid, &fw).await?;
                    return self.send_capsule_resp_err_async(cid, 0x83).await;
                }
            }
        }

        // Simplified wire (Unknown / Simplified)
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

    /// **V-followup-dhchap-3-wire / V-followup-dhchap-4** — 处理 AUTH_SEND：
    /// host 提交 NEGOTIATE / REPLY / HMAC response。
    ///
    /// **Wire 模式自动识别 (首个 AUTH_SEND, reviewer L-3 加严)**:
    /// - data.len() ≥ 12 且 `[0x01, 0x00]` 起头 且 `sc_c=0` 且 `napd ≥ 1`
    ///   → Spec4Msg (NEGOTIATE 最短合法 wire 是 12 B：8B header + 4B descriptor
    ///   header + 0 ids — 实际再加 idlist 才有意义。raw HMAC 32B 不可能满足
    ///   "起头 0x01,0x00 + 第 7 B = 0 + 第 8 B ≥ 1" 四条同时，碰撞 ≈ 2^-32)
    /// - 其他 → Simplified (老 V-followup-dhchap-3 路径，data = 32B HMAC)
    ///
    /// 一旦锁定，后续 AUTH_SEND 必须沿用同一 wire。
    ///
    /// reviewer H-1 修：Spec 路径所有解析错误现在都先发 FAILURE1 wire C2HData
    /// 再回 capsule 错，让 Linux nvme-cli 拿到 rescode_exp 诊断信息。
    async fn handle_auth_send_async(&mut self, cid: u16, data: &[u8]) -> anyhow::Result<()> {
        use crate::dhchap::{ChapStage, ChapWireMode};

        // V-followup-dhchap-4 reviewer borrow-fix: 所有 `chap` 借用集中于一个
        // scope；scope 退出后再做 async wire I/O，避免对 self 的 reborrow 冲突。
        // 返回 (Option<wire_failure_bytes>, capsule_sc, log_done)。
        enum AuthSendDecision {
            CapsuleErr(u8),                   // 简单 capsule err，无 wire
            WireFailThenCapsule(Vec<u8>, u8), // 先发 wire，再 capsule_sc
            CapsuleOk,                        // 简单 OK
        }

        let decision = {
            let neg = match self.chap.as_mut() {
                Some(n) => n,
                None => {
                    tracing::warn!("AUTH_SEND 拒：session 未 enable_chap");
                    return self.send_capsule_resp_err_async(cid, 0x83).await;
                }
            };

            // V-followup-dhchap-4 reviewer L-3: 4 条同时满足才认 Spec4Msg
            if neg.wire_mode == ChapWireMode::Unknown {
                let looks_spec = data.len() >= 12
                    && data[0] == crate::dhchap::wire::AUTH_TYPE_DHCHAP
                    && data[1] == crate::dhchap::wire::MSG_NEGOTIATE
                    && data[6] == 0           // sc_c == 0
                    && data[7] >= 1; // napd >= 1
                if looks_spec {
                    neg.wire_mode = ChapWireMode::Spec4Msg;
                    tracing::info!("V-dhchap-4: 锁定 Spec4Msg wire (host 发 NEGOTIATE)");
                } else {
                    neg.wire_mode = ChapWireMode::Simplified;
                    tracing::info!("V-dhchap-3-wire: 锁定 Simplified wire");
                }
            }

            // Spec 4-msg 路径
            if neg.wire_mode == ChapWireMode::Spec4Msg {
                if data.len() < 2 || data[0] != crate::dhchap::wire::AUTH_TYPE_DHCHAP {
                    tracing::warn!(
                        len = data.len(),
                        "AUTH_SEND (spec): auth_type 错或 truncated"
                    );
                    let fw = crate::dhchap::build_failure(
                        neg.spec_tid,
                        true,
                        crate::dhchap::wire::FAIL_EXP_INCORRECT_PAYLOAD,
                    );
                    AuthSendDecision::WireFailThenCapsule(fw, 0x02)
                } else {
                    match data[1] {
                        crate::dhchap::wire::MSG_NEGOTIATE => {
                            match crate::dhchap::parse_negotiate(data) {
                                Ok((tid, _, _)) => {
                                    neg.spec_tid = tid;
                                    tracing::info!(tid, "V-dhchap-4 NEGOTIATE accepted");
                                    AuthSendDecision::CapsuleOk
                                }
                                Err(e) => {
                                    let emsg = format!("{e:#}");
                                    let exp = if emsg.contains("SHA-256") {
                                        crate::dhchap::wire::FAIL_EXP_HASH_UNUSABLE
                                    } else if emsg.contains("DH NULL") {
                                        crate::dhchap::wire::FAIL_EXP_DHGROUP_UNUSABLE
                                    } else {
                                        crate::dhchap::wire::FAIL_EXP_INCORRECT_PAYLOAD
                                    };
                                    tracing::warn!(error = %e, exp, "V-dhchap-4 NEGOTIATE rejected");
                                    let fw = crate::dhchap::build_failure(0, true, exp);
                                    neg.stage = ChapStage::Failed;
                                    AuthSendDecision::WireFailThenCapsule(fw, 0x83)
                                }
                            }
                        }
                        crate::dhchap::wire::MSG_REPLY => {
                            // reviewer M-1: REPLY 必须在 ChallengeSent
                            if !matches!(&neg.stage, ChapStage::ChallengeSent { .. }) {
                                tracing::warn!(
                                    stage = ?neg.stage,
                                    "V-dhchap-4 REPLY 在非 ChallengeSent state，拒"
                                );
                                let fw = crate::dhchap::build_failure(
                                    neg.spec_tid,
                                    true,
                                    crate::dhchap::wire::FAIL_EXP_INCORRECT_MESSAGE,
                                );
                                neg.stage = ChapStage::Failed;
                                AuthSendDecision::WireFailThenCapsule(fw, 0x83)
                            } else {
                                match crate::dhchap::parse_reply(data) {
                                    Err(e) => {
                                        tracing::warn!(error = %e, "V-dhchap-4 REPLY parse fail");
                                        let fw = crate::dhchap::build_failure(
                                            neg.spec_tid,
                                            true,
                                            crate::dhchap::wire::FAIL_EXP_INCORRECT_PAYLOAD,
                                        );
                                        AuthSendDecision::WireFailThenCapsule(fw, 0x02)
                                    }
                                    Ok((tid, rval)) => {
                                        if tid != neg.spec_tid {
                                            tracing::warn!(
                                                got = tid,
                                                expect = neg.spec_tid,
                                                "V-dhchap-4 REPLY tid mismatch"
                                            );
                                            let fw = crate::dhchap::build_failure(
                                                neg.spec_tid,
                                                true,
                                                crate::dhchap::wire::FAIL_EXP_INCORRECT_PAYLOAD,
                                            );
                                            neg.stage = ChapStage::Failed;
                                            AuthSendDecision::WireFailThenCapsule(fw, 0x83)
                                        } else {
                                            let mut resp = [0u8; crate::dhchap::HMAC_SHA256_LEN];
                                            resp.copy_from_slice(&rval);
                                            let ok = neg.verify_host_response(&resp);
                                            if ok {
                                                tracing::info!("V-dhchap-4 REPLY verified");
                                                AuthSendDecision::CapsuleOk
                                            } else {
                                                tracing::warn!("V-dhchap-4 REPLY verify failed");
                                                let fw = crate::dhchap::build_failure(
                                                    neg.spec_tid,
                                                    true,
                                                    crate::dhchap::wire::FAIL_EXP_FAILED,
                                                );
                                                AuthSendDecision::WireFailThenCapsule(fw, 0x83)
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        crate::dhchap::wire::MSG_SUCCESS2 => {
                            if !neg.stage.is_authenticated() {
                                tracing::warn!(
                                    stage = ?neg.stage,
                                    "V-dhchap-4 SUCCESS2 在未认证 state，拒"
                                );
                                let fw = crate::dhchap::build_failure(
                                    neg.spec_tid,
                                    true,
                                    crate::dhchap::wire::FAIL_EXP_INCORRECT_MESSAGE,
                                );
                                neg.stage = ChapStage::Failed;
                                AuthSendDecision::WireFailThenCapsule(fw, 0x83)
                            } else {
                                tracing::info!("V-dhchap-4 SUCCESS2 received (unidirectional ack)");
                                AuthSendDecision::CapsuleOk
                            }
                        }
                        crate::dhchap::wire::MSG_FAILURE2 => {
                            tracing::warn!("V-dhchap-4 FAILURE2 received → state Failed");
                            neg.stage = ChapStage::Failed;
                            AuthSendDecision::CapsuleErr(0x83)
                        }
                        other => {
                            tracing::warn!(msg = other, "V-dhchap-4 unknown auth_id");
                            let fw = crate::dhchap::build_failure(
                                neg.spec_tid,
                                true,
                                crate::dhchap::wire::FAIL_EXP_INCORRECT_MESSAGE,
                            );
                            AuthSendDecision::WireFailThenCapsule(fw, 0x02)
                        }
                    }
                }
            } else {
                // Simplified wire
                if data.len() != crate::dhchap::HMAC_SHA256_LEN {
                    tracing::warn!(
                        got = data.len(),
                        expect = crate::dhchap::HMAC_SHA256_LEN,
                        "AUTH_SEND 拒：response 长度非法"
                    );
                    AuthSendDecision::CapsuleErr(0x02)
                } else {
                    let mut resp = [0u8; crate::dhchap::HMAC_SHA256_LEN];
                    resp.copy_from_slice(data);
                    let ok = neg.verify_host_response(&resp);
                    if ok {
                        tracing::info!("V-followup-dhchap-3-wire CHAP 通过");
                        AuthSendDecision::CapsuleOk
                    } else {
                        tracing::warn!(
                            stage = ?neg.stage,
                            "V-followup-dhchap-3-wire CHAP 校验失败"
                        );
                        AuthSendDecision::CapsuleErr(0x83)
                    }
                }
            }
        }; // ← `neg` borrow ends here

        match decision {
            AuthSendDecision::CapsuleOk => self.send_capsule_resp_ok_async(cid, 0).await,
            AuthSendDecision::CapsuleErr(sc) => self.send_capsule_resp_err_async(cid, sc).await,
            AuthSendDecision::WireFailThenCapsule(fw, sc) => {
                self.send_c2h_data_async(cid, &fw).await?;
                self.send_capsule_resp_err_async(cid, sc).await
            }
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
        use nvme_firmware::cmd::Sqe;

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

        // V5e-1-fix NS-shape 黑名单（allow_format opt-in 放行 Format）
        if matches!(
            decide_admin_blocked_opc(sqe_bytes, self.allow_format),
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
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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
        mut sqe: nvme_firmware::cmd::Sqe,
    ) -> anyhow::Result<()> {
        sqe.prp1 = crate::PRP1_SENTINEL;
        sqe.prp2 = 0;
        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        let conn_id = self.conn_id;
        let immediate = {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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
    ///
    /// **V-followup-prp-list (session chunking)** — host nlb > V5_NLB_MAX 时
    /// 不再 SC=0x18 reject；session 透明地把 IO 拆成 `ceil(N/V5_NLB_MAX)`
    /// 个 sub-cmd 各走 V5e-2 dual-PRP 路径，串行发 R2T (Write) /
    /// C2HData (Read)，最后给 host 单个 CapsuleResp。
    ///
    /// 优点：避开 controller PRP-list path 复杂性 (mixed read/write capture
    /// 重构)；缺点：host 看到一条 IO，target 在 backing 上发了 N 个串行
    /// dispatch — 单 host IO IOPS 受 N 倍延迟。教学版可接受；生产应做
    /// 真 PRP-list 让 controller 一次 dispatch 多 page。
    ///
    /// 单 cmd 上限受 spec MDTS 限制；当前 advertised MDTS=5 → 256 LBA = 128 KiB
    /// (与 V_HOST_IO_NLB_MAX 一致)。
    async fn handle_io_cmd_async(&mut self, cid: u16, sqe_bytes: &[u8]) -> anyhow::Result<()> {
        use crate::dispatch_plan::{IoNlbDecision, decide_io_nlb_check, prp2_sentinel_for_nlb};
        use nvme_firmware::cmd::Sqe;

        let mut sqe = Sqe::read_from_bytes(sqe_bytes)
            .map_err(|_| anyhow::anyhow!("V8e-7-3 IO SQE 不是 64 byte"))?;

        // V5b R-5：清 PSDT bits
        sqe.cdw0 &= !(0b11u32 << 14);

        // **2026-06-09 fused C&W over fabric** — fuse bits 9:8（1=FIRST/Compare、
        // 2=SECOND/Write）。fabric 此前走 `dispatch_io` 无 fuse 处理 → Compare/Write
        // 当两条独立命令执行 → 零 atomic CAS（Compare 失败 Write 仍写）。改由
        // session 独占配对 + hoist + controller `nvme_fused_cas` 原子 CAS。
        let fuse = ((sqe.cdw0 >> 8) & 0x3) as u8;
        if fuse != 0 {
            return self.handle_fused_io_async(cid, sqe, fuse).await;
        }
        // fuse=0 普通命令到达时若有未配对 FIRST → 打断它 abort（spec § 6.2：
        // SECOND 必须紧跟 FIRST，中间插入非 fused 命令则 FIRST 失效）。
        if let Some((_, old_cid)) = self.pending_fused.take() {
            self.send_capsule_resp_err_async(old_cid, /*INVALID_FIELD*/ 0x02)
                .await?;
        }

        sqe.prp1 = crate::PRP1_SENTINEL;

        let sq_id = self.current_qid;
        let cq_id = match self.io_queues.get(&sq_id) {
            Some(crate::io_queue::IoQueueState::Sq { cq_id, .. }) => *cq_id,
            _ => anyhow::bail!(
                "V8e-7-3 invariant: handle_io_cmd_async 但 current_qid={sq_id} 不是 IO SQ"
            ),
        };

        let opc = (sqe.cdw0 & 0xff) as u8;
        let nlb_real = (sqe.cdw12 & 0xffff) + 1;
        let is_rw = matches!(opc, 0x01 | 0x02);

        // **2026-06-09 纯 4K** — 查目标 NS 的 lbads，让页边界 / nlb 上限 /
        // chunk 大小按 per-NS 扇区算（512B 假设会在 4K NS 上撕裂 dual-PRP
        // 边界 → corruption）。NSID 不存在时退回 9（controller 随后会以
        // INVALID_NAMESPACE 拒）。
        let lbads = {
            let c = self.controller.controller.lock();
            c.ns_lbads(sqe.nsid).unwrap_or(9)
        };

        // **V-followup-prp-list session chunking** — host nlb 超单次 dispatch
        // (dual-PRP) 上限时拆 sub-cmd 并对 host_io_max_lbas (= MDTS) 真值上限校验
        if is_rw && nlb_real > crate::dispatch_plan::dual_prp_max_lbas(lbads) {
            if nlb_real > crate::dispatch_plan::host_io_max_lbas(lbads) {
                tracing::warn!(
                    opc,
                    nlb_real,
                    lbads,
                    cap = crate::dispatch_plan::host_io_max_lbas(lbads),
                    "V-followup-prp-list: nlb > MDTS cap, reject SC=0x18"
                );
                return self.send_capsule_resp_err_async(cid, 0x18).await;
            }
            return self
                .handle_io_cmd_chunked_async(cid, sqe, sq_id, cq_id, opc, nlb_real, lbads)
                .await;
        }

        // 分类 rw vs 非 rw（用早读 lbads 做路由足够）；prp2 的**最终**值
        // 与 ≤2 页守卫放到 dispatch 同一锁内用**复读** lbads 定，防 TOCTOU。
        let is_rw_single = matches!(decide_io_nlb_check(&sqe, lbads), IoNlbDecision::Ok { .. });

        let mut tcp_t =
            crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
        // **2026-06-09 纯 4K TOCTOU 防护** — 多 conn 共享一个 controller 时，
        // 另一条 conn 的 Format 可能在「早读 lbads」与「本 dispatch」之间改
        // lbads。controller 在 dispatch 时按**当时**的 ns.lbads 算 bytes；若与
        // session 据早读 lbads 定的 prp2 不一致，会让 controller 误入 PRP-list
        // path（合成-PRP 无法表达）→ wire 撕裂 / corruption。故在 dispatch 同
        // 一锁内复读 lbads：① 用它算 prp2；② 守 transfer ≤ 2 页（controller
        // 合成-PRP 上限）。扇区变大致超 2 页 → 中止让 host 重试（retryable）。
        let dispatch_result: Option<Option<_>> = {
            let mut c = self.controller.controller.lock();
            if is_rw_single {
                let lbads_now = c.ns_lbads(sqe.nsid).unwrap_or(9);
                if ((nlb_real as u64) << lbads_now)
                    > (2 * crate::dispatch_plan::NVME_PRP_PAGE_BYTES) as u64
                {
                    None // TOCTOU：扇区已变大，本 dispatch 会超 2 页 → 中止
                } else {
                    sqe.prp2 = prp2_sentinel_for_nlb(nlb_real, lbads_now);
                    let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                    Some(c.nvme_io_dispatch(&mut ctx, sq_id, sqe, cid, cq_id))
                }
            } else {
                sqe.prp2 = 0;
                let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                Some(c.nvme_io_dispatch(&mut ctx, sq_id, sqe, cid, cq_id))
            }
        };
        let Some(immediate_cqe) = dispatch_result else {
            tracing::warn!(
                cid,
                nlb_real,
                "纯 4K TOCTOU: lbads 在决策后被 Format 改大，本 dispatch 超 2 页，中止让 host 重试"
            );
            return self.send_capsule_resp_err_async(cid, 0x18).await;
        };
        self.run_post_dispatch_async(cid, immediate_cqe, tcp_t)
            .await
    }

    /// **2026-06-09 fused C&W over fabric** — 处理带 fuse bits 的 IO capsule，
    /// 做 **原子** Compare-and-Write（spec § 6.2）。
    ///
    /// session 独占 fuse 配对（`pending_fused`）：FIRST(Compare,fuse=01) 暂存不
    /// 响应；SECOND(Write,fuse=10) 到达 → 校验对齐 → **hoist**：先经 R2T 把
    /// Compare 与 Write 两个 host buffer 取齐，再单次调 controller `nvme_fused_cas`
    /// 在一把锁内 read-compare-write（真原子，不中途释放锁让别 conn 插队），回双
    /// CapsuleResp。FIRST 未配对即被打断（再来 FIRST / fuse=0 / 不匹配）→ 旧
    /// FIRST abort INVALID_FIELD；SECOND 无 FIRST / 非法 fuse=3 → INVALID_FIELD。
    async fn handle_fused_io_async(
        &mut self,
        cid: u16,
        sqe: nvme_firmware::cmd::Sqe,
        fuse: u8,
    ) -> anyhow::Result<()> {
        use nvme_firmware::cmd::nvm_opc;
        const SC_INVALID_FIELD: u8 = 0x02; // NVMe Generic: Invalid Field in Command

        match fuse {
            1 => {
                // FIRST 必须是 Compare（spec § 6.2 唯一定义的 fused pair）。
                if sqe.opcode() != nvm_opc::COMPARE {
                    return self
                        .send_capsule_resp_err_async(cid, SC_INVALID_FIELD)
                        .await;
                }
                // 已有未配对 FIRST → 被本 FIRST 打断，旧的 abort（spec：SECOND
                // 必须紧跟 FIRST）。
                if let Some((_, old_cid)) = self.pending_fused.take() {
                    self.send_capsule_resp_err_async(old_cid, SC_INVALID_FIELD)
                        .await?;
                }
                self.pending_fused = Some((sqe, cid));
                Ok(()) // 不响应，等 SECOND
            }
            2 => {
                let write_sqe = sqe;
                let write_cid = cid;
                let Some((compare_sqe, compare_cid)) = self.pending_fused.take() else {
                    // SECOND 无 FIRST → INVALID_FIELD。
                    return self
                        .send_capsule_resp_err_async(write_cid, SC_INVALID_FIELD)
                        .await;
                };
                // 校验对齐（session 需 size 发 R2T；nvme_fused_cas 再独立验一遍）。
                let nsid = compare_sqe.nsid;
                let slba = compare_sqe.cdw10 as u64 | ((compare_sqe.cdw11 as u64) << 32);
                let nlb = (compare_sqe.cdw12 & 0xffff) + 1;
                let w_slba = write_sqe.cdw10 as u64 | ((write_sqe.cdw11 as u64) << 32);
                let w_nlb = (write_sqe.cdw12 & 0xffff) + 1;
                if write_sqe.opcode() != nvm_opc::WRITE
                    || write_sqe.nsid != nsid
                    || w_slba != slba
                    || w_nlb != nlb
                {
                    // 两条都 INVALID_FIELD（spec § 6.2：fused 两条各 individual CQE）。
                    self.send_capsule_resp_err_async(compare_cid, SC_INVALID_FIELD)
                        .await?;
                    return self
                        .send_capsule_resp_err_async(write_cid, SC_INVALID_FIELD)
                        .await;
                }
                let sq_id = self.current_qid;
                let cq_id = match self.io_queues.get(&sq_id) {
                    Some(crate::io_queue::IoQueueState::Sq { cq_id, .. }) => *cq_id,
                    _ => anyhow::bail!("fused SECOND 但 current_qid={sq_id} 非 IO SQ"),
                };
                // host buffer 大小 = nlb × 当前扇区。
                let sector = {
                    let c = self.controller.controller.lock();
                    1u64 << c.ns_lbads(nsid).unwrap_or(9)
                };
                let bytes = (nlb as u64 * sector) as u32;
                // fused 上限单 PRP ≤ 1 page（controller fused gate 同）。超出 →
                // 双 INVALID_FIELD（优于让 dma_read R2T 因超 V4_MAX_DMA_READ_BYTES
                // bail 关连接；给 host 干净拒绝）。
                if bytes > crate::dispatch_plan::NVME_PRP_PAGE_BYTES {
                    self.send_capsule_resp_err_async(compare_cid, SC_INVALID_FIELD)
                        .await?;
                    return self
                        .send_capsule_resp_err_async(write_cid, SC_INVALID_FIELD)
                        .await;
                }
                // hoist：先取齐两 host buffer（各按自己 cccid 发 R2T），再原子 CAS。
                let compare_data = self
                    .dma_read_via_r2t_async(compare_cid, 0, bytes)
                    .await
                    .context("fused: R2T Compare 数据")?;
                let write_data = self
                    .dma_read_via_r2t_async(write_cid, 0, bytes)
                    .await
                    .context("fused: R2T Write 数据")?;
                let (c_cqe, w_cqe) = {
                    let mut c = self.controller.controller.lock();
                    c.nvme_fused_cas(
                        sq_id,
                        cq_id,
                        compare_sqe,
                        write_sqe,
                        &compare_data,
                        &write_data,
                    )
                };
                // 双 CapsuleResp（CQE bytes 各含自己 cccid，host 配对）。
                self.write_capsule_resp_bytes_async(c_cqe.as_bytes())
                    .await?;
                self.write_capsule_resp_bytes_async(w_cqe.as_bytes()).await
            }
            _ => {
                // fuse=3 reserved → INVALID_FIELD。
                self.send_capsule_resp_err_async(cid, SC_INVALID_FIELD)
                    .await
            }
        }
    }

    /// **V-followup-prp-list session chunking** — 大 IO 拆 sub-cmd。
    ///
    /// 把 nlb_real 拆成 ⌈nlb/V5_NLB_MAX⌉ 个 sub-cmd:
    /// - 每个 sub-cmd 用 unique sub-cid 让 controller 不撞 (host 的 cid 复用 OK)
    /// - 每个 sub-cmd 走 controller V5e-2 dual-PRP 单 dispatch
    /// - Read: 每 sub-cmd 收 controller 写 captured data → 我们发 1 个 C2HData
    /// - Write: 每 sub-cmd controller 要 dma_read → 走 R2T 三段式
    /// - 最后一个 sub-cmd 完成后用其 CQE 反给 host (cid 改回 host 的)
    ///
    /// 这避开了控制器 PRP-list path (mixed read/write capture 复杂度)，但
    /// 单 host IO 拆 N 次 dispatch — IOPS 降 N 倍。教学版可接受。
    #[allow(clippy::too_many_arguments)]
    async fn handle_io_cmd_chunked_async(
        &mut self,
        cid: u16,
        original_sqe: nvme_firmware::cmd::Sqe,
        sq_id: u16,
        cq_id: u16,
        opc: u8,
        nlb_real: u32,
        lbads: u8,
    ) -> anyhow::Result<()> {
        use crate::dispatch_plan::prp2_sentinel_for_nlb;
        // **2026-06-09 纯 4K** — chunk 大小 = 单次 dual-PRP 能覆盖的 LBA 数，
        // 按扇区算（512B→16、4K→2）。固定 16 会让 4K chunk = 64 KiB = 16 页
        // 撞穿 controller dual-PRP（≤ 2 页）→ corruption。
        let chunk_max = crate::dispatch_plan::dual_prp_max_lbas(lbads);
        let num_chunks = nlb_real.div_ceil(chunk_max);
        let slba = original_sqe.cdw10 as u64 | ((original_sqe.cdw11 as u64) << 32);

        tracing::info!(
            cid,
            opc,
            nlb_real,
            num_chunks,
            slba,
            "V-followup-prp-list: chunked IO dispatch start"
        );

        let mut _last_sc: u8 = 0;
        // **2026-06-09 纯 4K / chunked** — 累计 host-buffer 字节偏移，让每 chunk 的
        // R2T / C2HData 偏移是 host-buffer-relative（见 run_post_dispatch_chunked
        // _async base_offset）。sector 用 IO 起始 lbads（TOCTOU 改 lbads 会中止）。
        let sector_bytes: u32 = 1u32 << lbads;
        let mut host_buf_offset: u32 = 0;
        for chunk_idx in 0..num_chunks {
            let chunk_lba_off = chunk_idx * chunk_max;
            let chunk_nlb = chunk_max.min(nlb_real - chunk_lba_off);
            let chunk_slba = slba + chunk_lba_off as u64;

            // 构造 sub-SQE
            let mut sub_sqe = original_sqe;
            sub_sqe.cdw10 = (chunk_slba & 0xffff_ffff) as u32;
            sub_sqe.cdw11 = ((chunk_slba >> 32) & 0xffff_ffff) as u32;
            sub_sqe.cdw12 = (sub_sqe.cdw12 & !0xffff) | (chunk_nlb - 1); // 0-based
            // prp2 + ≤2 页守卫在 dispatch 同锁内用复读 lbads 定（防多 conn Format
            // 在分片中途改 lbads → 本片超 controller dual-PRP 上限 → wire 撕裂）。

            let mut tcp_t =
                crate::tcp_transport::TcpAdminTransport::new_with_token_base(self.next_token);
            let dispatch_result: Option<Option<_>> = {
                let mut c = self.controller.controller.lock();
                let lbads_now = c.ns_lbads(sub_sqe.nsid).unwrap_or(9);
                if ((chunk_nlb as u64) << lbads_now)
                    > (2 * crate::dispatch_plan::NVME_PRP_PAGE_BYTES) as u64
                {
                    None // TOCTOU：扇区中途变大，本片超 2 页 → 中止整条 host IO
                } else {
                    sub_sqe.prp2 = prp2_sentinel_for_nlb(chunk_nlb, lbads_now);
                    let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                    Some(c.nvme_io_dispatch(&mut ctx, sq_id, sub_sqe, cid, cq_id))
                }
            };
            let Some(immediate_cqe) = dispatch_result else {
                tracing::warn!(
                    cid,
                    chunk_idx,
                    chunk_nlb,
                    "纯 4K TOCTOU: 分片 IO 中途 lbads 被 Format 改大，中止让 host 重试"
                );
                return self.send_capsule_resp_err_async(cid, 0x18).await;
            };

            // 复用 V8e-7-3 R2T 三段式 + data emit。**关键**: 不让最终 CQE 反给
            // host (拦截掉 capsule_resp emit 由 chunked driver 控制)。
            // run_post_dispatch_async 内部会发 capsule resp；为了 chunked 模式
            // 我们手动跑 phase 2 (R2T) + phase 3 (post_cqe) + phase 4 (data emit)
            // 但跳过 capsule resp emit。
            let chunk_sc = self
                .run_post_dispatch_chunked_async(
                    cid,
                    immediate_cqe,
                    tcp_t,
                    /*emit_capsule_resp*/ chunk_idx == num_chunks - 1,
                    host_buf_offset,
                )
                .await?;
            if chunk_sc != 0 {
                _last_sc = chunk_sc;
                // 早 fail：剩余 sub-cmd 不发；emit 最终 err resp
                self.send_capsule_resp_err_async(cid, chunk_sc).await?;
                tracing::warn!(
                    cid,
                    chunk_idx,
                    chunk_sc = format_args!("{:#x}", chunk_sc),
                    "V-prp-list chunked sub-cmd fail; abort"
                );
                return Ok(());
            }
            // 本 chunk 成功 → 推进 host-buffer 偏移（chunk_nlb LBA × sector）。
            host_buf_offset = host_buf_offset.saturating_add(chunk_nlb * sector_bytes);
        }
        tracing::debug!(cid, num_chunks, "V-prp-list chunked done");
        Ok(())
    }

    /// **V-followup-prp-list** — chunked 版本的 run_post_dispatch_async。
    ///
    /// 与 [`Self::run_post_dispatch_async`] 区别：
    /// - 不 emit CapsuleResp (chunked 完成后 caller 决定)
    /// - 仍 emit C2HData / R2T (每 chunk 自身的 wire effect 必须发)
    /// - 返 chunk 的 SC byte (0 = OK，非 0 = err，caller 短路)
    async fn run_post_dispatch_chunked_async(
        &mut self,
        cid: u16,
        immediate_cqe: Option<nvme_firmware::cmd::Cqe>,
        mut tcp_t: crate::tcp_transport::TcpAdminTransport,
        emit_capsule_resp: bool,
        base_offset: u32,
    ) -> anyhow::Result<u8> {
        // 走 run_post_dispatch_async 大部分逻辑，但 phase 5 emit 时按 flag 决定
        let dispatch_data_writes = tcp_t
            .writes
            .iter()
            .filter(|w| w.gpa < crate::CQ_BASE_GPA)
            .count();
        let dispatch_pending_reads = tcp_t.pending_reads.len();
        if dispatch_data_writes > 0 && dispatch_pending_reads > 0 {
            anyhow::bail!(
                "V-prp-list chunked: chunk produced both data_write ({}) and dma_read ({})",
                dispatch_data_writes,
                dispatch_pending_reads
            );
        }
        let had_pending_reads = dispatch_pending_reads > 0;

        // Phase 2: dma_read 闭环走 R2T 三段式
        // **2026-06-09 纯 4K / chunked** — 从 base_offset（前序 chunk 的累计 host
        // 字节）起，让本 chunk 的 R2T 偏移是 **host-buffer-relative**；否则每 chunk
        // 从 0 起 → host 把第 N 片数据当第 0 片发 → 多片 WRITE 数据 corruption。
        let mut cmd_cumulative_offset: u32 = base_offset;
        while let Some(read_req) = tcp_t.pop_read() {
            let bytes = self
                .dma_read_via_r2t_async(cid, cmd_cumulative_offset, read_req.len)
                .await
                .with_context(|| {
                    format!(
                        "V-prp-list chunked dma_read failed (cid={cid}, token={tok}, len={l}, offset={off})",
                        tok = read_req.token,
                        l = read_req.len,
                        off = cmd_cumulative_offset,
                    )
                })?;
            cmd_cumulative_offset = cmd_cumulative_offset.saturating_add(read_req.len);
            {
                let mut c = self.controller.controller.lock();
                let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                c.nvme_admin_complete_dma(&mut ctx, read_req.token, true, bytes);
            }
        }

        // Phase 3: sync / async write-out
        if let Some(cqe) = immediate_cqe {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
            c.nvme_post_cqe(&mut ctx, cqe);
        } else if !had_pending_reads {
            let mut data_tokens = Vec::with_capacity(tcp_t.writes.len());
            for w in tcp_t.writes.iter() {
                if w.gpa >= crate::CQ_BASE_GPA {
                    anyhow::bail!(
                        "V-prp-list chunked: dispatch produced CQE write before on_dma_complete (gpa={:#x})",
                        { w.gpa }
                    );
                }
                data_tokens.push(w.token);
            }
            for tok in data_tokens {
                let mut c = self.controller.controller.lock();
                let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                c.nvme_admin_complete_dma(&mut ctx, tok, true, Vec::new());
            }
        }

        self.next_token = tcp_t.token_high_water();

        // Phase 4: drain captured.writes → data + cqe
        let mut data_payload = Vec::new();
        let mut cqe_bytes: Option<Vec<u8>> = None;
        while let Some(w) = tcp_t.pop_write() {
            if w.gpa >= crate::CQ_BASE_GPA {
                if cqe_bytes.is_some() {
                    anyhow::bail!("V-prp-list chunked: multiple CQE writes for single sub-cmd");
                }
                if w.data.len() != 16 {
                    anyhow::bail!("captured CQE write len={} != 16", w.data.len());
                }
                cqe_bytes = Some(w.data);
            } else {
                if cqe_bytes.is_some() {
                    anyhow::bail!(
                        "V-prp-list chunked: data write after CQE write (gpa={:#x}, {}B)",
                        { w.gpa },
                        w.data.len()
                    );
                }
                data_payload.extend_from_slice(&w.data);
            }
        }
        let cqe_bytes = cqe_bytes
            .ok_or_else(|| anyhow::anyhow!("V-prp-list chunked: controller did not produce CQE"))?;

        // 解 CQE SC byte
        let status = u16::from_le_bytes([cqe_bytes[14], cqe_bytes[15]]);
        let sc = ((status >> 1) & 0xFF) as u8;

        // Phase 5: emit C2HData (本 chunk 的 data 必须发) +
        // (按 flag 决定是否 emit CapsuleResp)
        // **2026-06-09** — C2HData 带 base_offset（host-buffer-relative），让
        // spec-compliant host 按 DATAO 把每片落对位（否则多片 READ corruption）。
        // DATA_LAST 仅打在末 chunk（= emit_capsule_resp 同条件）。
        if !data_payload.is_empty() {
            self.send_c2h_data_at_async(cid, &data_payload, base_offset, emit_capsule_resp)
                .await?;
        }
        if emit_capsule_resp {
            self.write_capsule_resp_bytes_async(&cqe_bytes).await?;
        }
        Ok(sc)
    }

    /// **V8e-7-3** — async run_post_dispatch（与 sync `run_post_dispatch`
    /// 1:1 等价；R2T loop 走 V8e §3 Q5 三段式：lock-pop / unlock-await-wire /
    /// lock-complete）。
    async fn run_post_dispatch_async(
        &mut self,
        cid: u16,
        immediate_cqe: Option<nvme_firmware::cmd::Cqe>,
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
                let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
                c.nvme_admin_complete_dma(&mut ctx, read_req.token, true, bytes);
            }
        }

        // Phase 3: 同步 / 异步 write-out
        if let Some(cqe) = immediate_cqe {
            let mut c = self.controller.controller.lock();
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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
                let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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

    /// **V9 R2** — 委托 `TcpFabricBackend::move_host_to_local`（R2T+H2CData 逻辑已迁入
    /// `fabric_backend`）。保留本签名（返 `Vec`）让 caller 不变；内部填入 dst 后返回。
    async fn dma_read_via_r2t_async(
        &mut self,
        cid: u16,
        base_offset: u32,
        len: u32,
    ) -> anyhow::Result<Vec<u8>> {
        use crate::fabric_backend::{FabricBackend as _, HostBuf};
        // **cap-before-allocate**（rust-reviewer HIGH-1）：先验上界再分配，防 pathological len
        // 先吃一坨内存（backend 内也有同款防御，此处保留旧的「分配前拒」顺序）。
        if len > crate::session::V4_MAX_DMA_READ_BYTES {
            anyhow::bail!(
                "R2 dma_read len {len} exceeds cap {}",
                crate::session::V4_MAX_DMA_READ_BYTES
            );
        }
        let mut dst = vec![0u8; len as usize];
        let host = HostBuf::FlowControlled { total_len: len };
        self.backend
            .move_host_to_local(cid, &host, base_offset, &mut dst)
            .await?;
        Ok(dst)
    }

    /// **V8e-7-3** — 发 C2HData PDU（一次性 + DATA_LAST），data_offset=0。
    /// 适用单次 transfer（admin / 非 chunked IO）：单 PDU 即末片，LAST=1。
    async fn send_c2h_data_async(&mut self, cid: u16, data: &[u8]) -> anyhow::Result<()> {
        self.send_c2h_data_at_async(cid, data, 0, /*is_last*/ true)
            .await
    }

    /// **2026-06-09 纯 4K / chunked** — 带 host-buffer `data_offset` + `is_last`
    /// 的 C2HData。**V9 R2** 委托 `TcpFabricBackend::move_local_to_host`（C2HData 逻辑迁入）。
    /// chunked READ 每片 C2HData 必须带**累计** host 偏移；`is_last` 仅打在整条命令最后一片。
    async fn send_c2h_data_at_async(
        &mut self,
        cid: u16,
        data: &[u8],
        data_offset: u32,
        is_last: bool,
    ) -> anyhow::Result<()> {
        use crate::fabric_backend::{FabricBackend as _, HostBuf};
        let host = HostBuf::FlowControlled {
            total_len: data.len() as u32,
        };
        self.backend
            .move_local_to_host(cid, &host, data, data_offset, is_last)
            .await
    }

    /// **V8e-7-3** — 把 controller 已 dma_write 的 16-byte CQE bytes 当
    /// CapsuleResp PSH 直接 emit（spec：RSP PDU PSH = CQE）。
    async fn write_capsule_resp_bytes_async(&mut self, cqe_bytes: &[u8]) -> anyhow::Result<()> {
        debug_assert_eq!(cqe_bytes.len(), 16);
        let cqe: [u8; 16] = cqe_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("CQE bytes len {} != 16", cqe_bytes.len()))?;
        use crate::fabric_backend::FabricBackend as _;
        self.backend.send_response_capsule(&cqe).await
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
                    let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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
            let mut ctx = pcie_device_core::DeviceCtx::new(&mut tcp_t);
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
