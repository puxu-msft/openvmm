// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V** — NVMe-over-Fabrics TCP target。
//!
//! 把 [`nvme_firmware::NvmeController`] 暴露在 TCP port 4420，
//! 任何安装了 nvme-tcp host 驱动（Linux ≥ 5.0 / Windows Server 2025）的机器
//! `nvme connect -t tcp -a <host> -n <NQN>` 即可挂载。
//!
//! 当前进度：
//! - **V0** ✅ controller crate 拆 lib+bin（外部 crate 可 import）
//! - **V1** ✅ PDU 数据结构 + 编解码 + CRC32C digest + 同步 TCP framing
//! - **V2** ✅ ICReq/ICResp 握手 + Fabric Connect / Property Get/Set
//!   + minimal session state machine
//! - **V3** ✅ admin cmd 派发到 NvmeController：Identify/Get Log Page
//!   等异步路径 captured dma_write 转 C2HData + CapsuleResp
//! - **V4a** ✅ wire layer：R2T encode + H2CData reassembler + TTAG 分配器
//! - **V4b** ✅ controller dma_read → R2T → H2CData 闭环（单段 ≤ 64 KiB）
//! - **V4c** ✅ MAXH2CDATA 分片 + 多 R2T 串行（dma_read > 64 KiB 自动切片）
//! - **V5a** ✅ IO queue 安装 + Fabric Connect qid≥1 + dispatch 二分
//! - **V5b** ✅ IO Read nlb=1 走 C2HData 闭环
//! - **V5c** ✅ IO Write nlb=1 走 R2T/H2CData 闭环（含数据持久化验证）
//! - **V5d** ✅ `main.rs` TcpListener:4420 + README + bin smoke test
//! - **V5d-fix / V5d-fix-2** ✅ security hardening: max-conn cap / loopback
//!   default / ctrlc graceful / handshake timeout / SIGPIPE invariant
//! - **V5e-1** ✅ nlb 上限放宽到 8（单 PRP1 4 KiB；Linux dd bs=4k 单 cmd 完成）
//! - **V5e-1-fix** ✅ session block FORMAT_NVM/NS_MANAGEMENT 防 lbads 漂变 + ctrlc hard-fail
//! - **V5e-2** ✅ 多 PRP 直接指针：nlb 扩到 16 (8 KiB)；PRP2_SENTINEL + cmd 累计 R2T offset
//! - **V6a** ✅ AER cmd fast-path (不 bail session) + ASYNC_LIMIT_EXCEEDED 上限保护 + Set FID 0x0B wire test
//! - **V6b** ✅ select-style pump_one_with_events + inject_aen API + ReadTimeout framing variant
//! - **V6c** ✅ AER 端到端 (FIFO 顺序 / 超 pending drop / Identify 交错) + README V6 章节
//! - **V7a** ✅ Discovery Log Page (LID 0x70) builder + controller wrapper
//! - **V7b** ✅ session `discovery_mode` derive + Connect NQN 校验 + admin opc 白名单 + bin `--discovery-mode`
//! - **V7c-fix** ✅ Identify Ctrl CNTRLTYPE=0x02 + NN=0 patch (review H-1) + IO Connect explicit reject (H-2) + dead code clean
//! - **V8** 计划中
//! - **V-followup-tls-1** ✅ stream 抽象泛化 `AsyncSession<S: AsyncSessionStream>`；
//!   default type param 保兼容
//! - **V-followup-tls-2** ✅ rustls 0.23 + tokio-rustls 0.26 dep + `build_acceptor_from_pem`
//! - **V-followup-tls-3** ✅ bin TLS dual-listener（`--tls-listen` + 双 explicit consent
//!   + `TLS_HANDSHAKE_TIMEOUT_SECS=30` 防 slowloris + 不 fallback plaintext 防 downgrade）
//! - **V-followup-tls-4** ✅ 应用层 byte-identical gate（plaintext vs TLS 解密后等价）
//!   + README "TLS 教学开关" 章节
//! - **V-followup-mtls** ✅ `build_acceptor_with_mtls` + `--tls-client-ca`
//!   CLI；`WebPkiClientVerifier` 强制 client cert chain 锚到 trust roots
//! - **V-followup-auth** ✅ host NQN 白名单：`--allow-host-nqn` 可重复 +
//!   `accept_and_handshake_async_with_auth` + Connect 时返
//!   `CONNECT_INVALID_HOST` (0x84) 给未授权 hostnqn
//! - **V-followup-auth-2** ✅ NQN ↔ TLS cert identity binding（spec section
//!   8.13）：`--tls-bind-nqn-to-cert` 启 mTLS leaf cert SAN URI/DNS/CN 抽取
//!   作为 host identity；Connect 时强制 hostnqn ∈ identities
//! - **V-followup-dhchap-1** ✅ DH-HMAC-CHAP 算法 building block (HMAC-only)：
//!   challenge / response 计算 / constant-time verify / secret store；wire
//!   集成 (AUTH_SEND/RECV state machine) 留 V-followup-dhchap-2
//! - **V-followup-dhchap-2** ✅ `ChapStage` / `ChapNegotiation` state machine
//!   + AsyncSession 集成（`chap_secret_store` / `chap` 字段 + `enable_chap`
//!     setter + Connect post-action 自动 init）；wire AUTH_SEND/RECV PDU
//!     dispatch 留 V-followup-dhchap-3
//! - **V-followup-dhchap-3** ✅ bin CLI `--host-secret <NQN>=<HEX>` 可重复 +
//!   `parse_host_secret` 参数校验 + 多 conn 共享 `Arc<ChapSecretStore>` +
//!   主/discovery/TLS 三 accept loop 透传；AUTH_SEND/RECV PDU wire encode +
//!   admin cmd gate 留 V-followup-dhchap-3-wire 后续 phase
//! - **V-followup-dhchap-3-wire** ✅ AUTH_SEND/AUTH_RECV wire dispatch
//!   (`handle_auth_recv_async` / `handle_auth_send_async`) + admin/IO cmd
//!   gate (`stage.is_authenticated()` 否则 SC=0x83)；完整 host->target HMAC
//!   challenge-response 闭环 e2e 通过

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// **V8e-3 (plan R-2/R-3)** — `tokio::sync::Mutex` guard 持锁跨 await 编译过却
// 死锁；`parking_lot::Mutex` guard 持锁跨 await 会卡 worker thread。两者都靠
// `await_holding_lock` lint 编译期防御。`with_controller` API 故意 closure-only
// 不传 future 强制锁作用域不跨 await（V8b plan R-1 在 async 下的强化）。
#![deny(clippy::await_holding_lock)]
#![warn(clippy::await_holding_refcell_ref)]

pub mod aer;
pub mod async_session;
pub mod dhchap;
pub mod digest;
pub mod dispatch_plan;
pub mod fabric;
pub mod fabric_backend;
pub mod framing;
pub mod h2c_reassembler;
pub mod io_queue;
pub mod pdu;
pub mod r2t;
pub mod rdma_backend;
pub mod rdma_cm;
pub mod session;
pub mod tcp_transport;
pub mod tls;
pub mod tls_identity;
/// **V-followup-tls-psk (TP-8011)** — NVMe TLS PSK 派生 (digest + HKDF-Expand-Label
/// + identity 字符串)。deterministic crypto only — 实际注入 rustls 待 upstream
/// external-PSK API。
pub mod tls_psk;
pub mod ttag;

pub use async_session::AsyncSession;
pub use async_session::AsyncSessionStream;
pub use async_session::DispatchOutcome;
pub use async_session::PumpEvent;
pub use async_session::accept_and_handshake_async;
pub use async_session::accept_and_handshake_async_with_auth;
pub use async_session::accept_rdma;
pub use async_session::ic_handshake_async;
pub use digest::crc32c;
pub use fabric::ConnectData;
pub use fabric::ConnectFabricFields;
pub use fabric::FabricError;
pub use fabric::PropertyFabricFields;
pub use framing::Pdu;
pub use framing::read_pdu;
pub use framing::read_pdu_async;
pub use framing::write_pdu;
pub use framing::write_pdu_async;
pub use h2c_reassembler::{AcceptOutcome, H2cReassembler};
pub use pdu::*;
pub use r2t::encode_r2t;
pub use session::ADMIN_CQ_SIZE;
pub use session::CQ_BASE_GPA;
pub use session::DISCOVERY_NQN;
pub use session::MAXH2CDATA_BYTES;
pub use session::NegotiatedIc;
pub use session::PRP1_SENTINEL;
pub use session::V_HOST_IO_NLB_MAX;
pub use session::V2Session;
pub use session::V5_NLB_MAX;
pub use session::ic_handshake;
pub use tls::{build_acceptor_from_pem, build_acceptor_with_mtls};
pub use tls_identity::extract_host_identities;

/// **Phase V8b** — 多 conn 共享 controller 的 wrapper（reviewer C-1 / M-1）。
///
/// - `controller`: 整 controller 一把 `parking_lot::Mutex`（仓库 clippy 禁
///   std::sync::Mutex；parking_lot 无 poisoning）。
/// - `next_conn_token_base`: per-conn token slab 起点的原子计数器。每条新 conn
///   在 `accept_and_handshake_shared` 内 `fetch_add(TOKEN_SLAB_SIZE)` 拿到自己
///   disjoint 的 token 段（slab 大小 1<<40，64-bit 空间够 ~16M conn）；
///   解决 reviewer C-1：多 conn 并发握手或稳态都不会因 controller `pending_ios`
///   高水位"被 complete 跌回 0"而起点撞 key → 跨 conn `PendingIo` 覆盖 →
///   数据破坏 / DoS。
pub struct SharedControllerInner {
    /// controller 整把短锁。
    pub controller: parking_lot::Mutex<nvme_firmware::NvmeController>,
    /// per-conn token slab base 全局原子分配。起值见 [`TOKEN_SLAB_START`]。
    pub next_conn_token_base: std::sync::atomic::AtomicU64,
    /// **V8c** — per-conn ID 全局原子分配（1 起；0 保留为 legacy/无关联）。
    /// `accept_and_handshake_shared` 内 `fetch_add(1)` 拿；用于 AER per-conn
    /// 路由（`nvme_admin_dispatch_with_conn` / `nvme_fire_aen_for_conn` /
    /// `nvme_cleanup_conn_aers`）。
    pub next_conn_id: std::sync::atomic::AtomicU32,
    /// **V8e-4 (plan Q3)** — AER wakeup channel：controller `aen_pending` 状态变
    /// 化时（push 入队 / fire 出队）任何调 [`Self::notify_aer`] 的 caller 触发；
    /// async session `pump_one_async` 在 select! 里 `notified().await` 拿 wakeup
    /// 后再自检 `nvme_pending_aer_count_for_conn(self.conn_id)` 决定是否真
    /// drain（plan §3 Q3：controller-wide 单 Notify + per-conn 自检；不需要
    /// per-conn 多 Notify 因 spurious wakeup 成本 = 1 atomic load + 1 short lock）。
    ///
    /// sync `V2Session::pump_one_with_events` 不依赖此 channel（保 100ms tick
    /// 兜底）；只有 `AsyncSession::pump_one_async` 受益。
    pub aen_notify: std::sync::Arc<tokio::sync::Notify>,
}

/// per-conn token slab 起始 base（保 V3/V4 测试日志 `1<<48` 习惯）。
pub const TOKEN_SLAB_START: u64 = 1u64 << 48;
/// per-conn token slab 大小（1<<40 ≈ 1T tokens / conn；64-bit 空间 ~16M conn）。
pub const TOKEN_SLAB_SIZE: u64 = 1u64 << 40;

impl SharedControllerInner {
    /// 从 owned controller 构造 shared wrapper；token base 起 [`TOKEN_SLAB_START`]，
    /// conn_id 起 1（0 保留为 legacy/无关联）。
    pub fn new(controller: nvme_firmware::NvmeController) -> Self {
        Self {
            controller: parking_lot::Mutex::new(controller),
            next_conn_token_base: std::sync::atomic::AtomicU64::new(TOKEN_SLAB_START),
            next_conn_id: std::sync::atomic::AtomicU32::new(1),
            aen_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// **V8e-4** — 主动 wakeup 所有 `notified().await` 的 task 让其重新查
    /// `nvme_pending_aer_count_for_conn(self.conn_id)`。
    ///
    /// 当 caller 通过 [`Self::with_aer_notify`] 调 controller 路径时自动调用；
    /// 测试 / 直接 push 路径也可手调。`notify_waiters` 语义：只唤醒 **当前**
    /// 在 wait 的 task；之前在 wait 的拿到一次性 permit。spurious wakeup 由
    /// session 自检 conn 计数过滤（plan §3 Q3）。
    pub fn notify_aer(&self) {
        self.aen_notify.notify_waiters();
    }

    /// **V8e-4** — 拿 Notify 的 cloned Arc。session `accept_and_handshake_async`
    /// 时 clone 给 `AsyncSession.aen_notify` 字段；后者 select! 里直接
    /// `aen_notify.notified()`。
    pub fn aen_notify_handle(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.aen_notify)
    }

    /// 给一条新 conn 分配 disjoint token slab base；slab size = [`TOKEN_SLAB_SIZE`]。
    /// 返回的 base 已是本 conn 第一条 cmd 的 `next_token` 起点。
    pub fn allocate_token_slab(&self) -> u64 {
        self.next_conn_token_base
            .fetch_add(TOKEN_SLAB_SIZE, std::sync::atomic::Ordering::SeqCst)
    }

    /// **V8c** — 给一条新 conn 分配全局 `conn_id`（1 起；0 保留 legacy）。
    /// **V8c reviewer H-2** — u32 wrap 后再 `fetch_add` 跳过 0；不去重活跃
    /// id（理论 ~16M conn 后撞旧 id 风险，V9 应改 u64 或加 live HashSet）。
    pub fn allocate_conn_id(&self) -> u32 {
        loop {
            let id = self
                .next_conn_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if id != 0 {
                return id;
            }
            // u32 wrap 到 0 → 再 fetch_add 拿下一个非 0 值
        }
    }
}

/// **Phase V8b** — `Arc<SharedControllerInner>` 别名让多 conn 共享 controller +
/// per-conn token slab 分配器。bin startup 一次 `NvmeController::open` →
/// `Arc::new(SharedControllerInner::new(...))` → 每条 conn `Arc::clone` 给
/// `V2Session::accept_and_handshake_shared`。
///
/// V8e tokio refactor 时改 controller field 为 `tokio::sync::Mutex<NvmeController>`。
pub type SharedController = std::sync::Arc<SharedControllerInner>;
pub use tcp_transport::TcpAdminTransport;
pub use ttag::TtagAllocator;
