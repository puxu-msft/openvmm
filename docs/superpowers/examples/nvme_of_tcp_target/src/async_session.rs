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
use tokio::net::TcpStream as TokioStream;
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
    Ok(AsyncSession {
        stream,
        controller,
        negotiated,
        conn_id,
        next_token,
        discovery_mode,
    })
}

impl AsyncSession {
    /// **V8e-3** — async 主循环单次 tick。
    ///
    /// V8e-3 仅 2 arm：
    /// 1. `read_pdu_async` → 占位 dispatch（V8e-4 加 AER Notify；V8e-5 加 KATO；
    ///    V8e-6 加完整 admin/IO dispatch；本 phase 仅打 log + drop PDU 走通流程）
    /// 2. `shutdown.changed()` → 返 `Ok(None)` 让 caller 退出 loop
    ///
    /// 返：
    /// - `Ok(Some(pdu))` 收到一帧（V8e-3 后调用 caller 不动它；V8e-6 后改 dispatch）
    /// - `Ok(None)` shutdown / peer close
    /// - `Err(_)` 协议 / IO 错
    pub async fn pump_one_async(
        &mut self,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<Option<Pdu>> {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                tracing::info!(conn_id = self.conn_id, "V8e-3 session shutdown");
                Ok(None)
            }
            r = read_pdu_async(&mut self.stream) => {
                match r {
                    Ok(pdu) => Ok(Some(pdu)),
                    Err(e) => {
                        // PeerClosed 走 Ok(None)；其余 Err 上抛
                        if let Some(crate::framing::FramingError::PeerClosed { .. }) =
                            e.downcast_ref::<crate::framing::FramingError>()
                        {
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                }
            }
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
