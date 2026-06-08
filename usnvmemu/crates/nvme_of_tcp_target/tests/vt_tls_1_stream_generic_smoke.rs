// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V-followup-tls-1 集成测试** — `AsyncSession<S>` 泛化后通过
//! `tokio::io::duplex` in-memory pair 验证：无 TCP 也能跑握手 + ICReq/ICResp。
//! 为后续 V-followup-tls-3 喂 `TlsStream` 提供 zero-touch 通路。
//!
//! 覆盖：
//! 1. `vt_tls_1_async_session_compiles_with_tokio_duplex` — 编译 + 行为：
//!    `AsyncSession<DuplexStream>` 完整握手成功；conn_id / token slab / KATO
//!    都正常分配
//! 2. `vt_tls_1_async_session_default_type_param_is_tcpstream` — 编译期反
//!    回归：default type param 真为 `TokioStream`（V8e-7 既有调用点 0 改动）
//! 3. `vt_tls_1_dispatch_pdu_over_duplex_smoke` — duplex 上完整 Connect →
//!    Property Get 流程；server 端走 **wire** 读 PDU 喂 `dispatch_pdu_async`，
//!    与主线程 client write 严格 1:1 配对，避免提早 drop session 触发
//!    `BrokenPipe`（reviewer H-1）
//! 4. `vt_tls_1_dyn_async_session_stream_object_safe` — 反回归编译期：
//!    防止未来给 `AsyncSessionStream` 加非对象安全方法 / 关联类型时无声破坏

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, AsyncSessionStream, SharedControllerInner, accept_and_handshake_async,
};
use std::sync::Arc;
use tokio::io::{DuplexStream, duplex};
use zerocopy::IntoBytes;

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

async fn send_icreq(s: &mut DuplexStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh::default();
    write_pdu_async(s, &hdr, psh.as_bytes(), &[]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_1_async_session_compiles_with_tokio_duplex() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = duplex(64 * 1024);

    let s = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let sess: AsyncSession<DuplexStream> = accept_and_handshake_async(server, s).await.unwrap();
        (sess.conn_id, sess.next_token)
    });

    send_icreq(&mut client).await;
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::ICRESP);

    let (conn_id, next_token) = h.await.unwrap();
    assert!(conn_id >= 1, "AsyncSession<DuplexStream> 应正常分 conn_id");
    assert!(
        next_token >= 1u64 << 48,
        "token slab base 应在 V8b TOKEN_SLAB_START 之后"
    );
    drop(client);
}

/// **reviewer M-1 修复** — 强制 default type param 单态化校验。
///
/// 旧版 `_assert_default<F>()` 永不实例化，编译器对未实例化的泛型函数不做完整
/// bound check，等于空测。改用 type ascription：把 `AsyncSession`（不写类型参
/// 数）与 `AsyncSession<tokio::net::TcpStream>` 拼成同一 `Option`，类型不一致
/// 即编译失败。
#[test]
fn vt_tls_1_async_session_default_type_param_is_tcpstream() {
    // 同一变量绑定两次类型表达式 → 编译器强制等价。
    let _slot: Option<AsyncSession> = None;
    let _slot2: Option<AsyncSession> = None::<AsyncSession<tokio::net::TcpStream>>;
    // 若 default param 改成非 TcpStream，第二行立即编译失败。
    // 让编译器看见使用，避免 unused warning。
    drop(_slot);
    drop(_slot2);
}

/// **reviewer H-1 修复** — 完整 wire 端到端：server 端从 stream 读 PDU 而非本
/// 地构造 cmd 喂 dispatch；与主线程 write 1:1 配对，消除 race。
#[tokio::test(flavor = "multi_thread")]
async fn vt_tls_1_dispatch_pdu_over_duplex_smoke() {
    let (shared, _backing) = make_shared();
    let (mut client, server) = duplex(64 * 1024);

    let s = Arc::clone(&shared);
    let h = tokio::spawn(async move {
        let mut sess: AsyncSession<DuplexStream> =
            accept_and_handshake_async(server, s).await.unwrap();
        // 用公开 API pump_one_async 从 wire 读 PDU；watch channel 用作占位
        // shutdown 通道（plan §3 Q4 设计），本测试不发 shutdown。
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        // 严格 2 帧配对主线程 write
        for _ in 0..2 {
            match sess.pump_one_async(&mut rx).await.unwrap() {
                nvme_of_tcp_target::PumpEvent::Pdu(p) => {
                    let _ = sess.dispatch_pdu_async(p).await.unwrap();
                }
                other => panic!("unexpected pump event: {:?}", other),
            }
        }
    });

    // 1. ICReq → ICResp
    send_icreq(&mut client).await;
    let ic_resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(ic_resp.header.pdu_type, pdu_type::ICRESP);
    // 2. Connect admin → RSP
    let connect = build_connect_pdu(0x0001);
    write_pdu_async(&mut client, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let connect_resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(connect_resp.header.pdu_type, pdu_type::RSP);
    // 3. Property Get CAP → RSP
    let pg = build_property_get_cap_pdu(0x0010);
    write_pdu_async(&mut client, &pg.header, &pg.psh, &pg.data)
        .await
        .unwrap();
    let pg_resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(pg_resp.header.pdu_type, pdu_type::RSP);

    drop(client);
    h.await.unwrap();
}

/// **reviewer M-2 修复** — 名实对齐，真验 `dyn AsyncSessionStream` 对象安全。
///
/// 防止未来给 `AsyncSessionStream` 加 `Self: Sized` 方法 / 关联类型 / 泛型
/// method 等让 trait 失去对象安全。即便实际部署走泛型 monomorphize，本测试
/// 守 trait 形状本身。
#[test]
fn vt_tls_1_dyn_async_session_stream_object_safe() {
    // 类型一旦不对象安全，下行编译即失败。
    let _: Option<Box<dyn AsyncSessionStream>> = None;
}

fn build_connect_pdu(cid: u16) -> Pdu {
    use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 0,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        },
        psh: sqe,
        data: cd.as_bytes().to_vec(),
    }
}

fn build_property_get_cap_pdu(cid: u16) -> Pdu {
    use nvme_of_tcp_target::fabric::{self, PropertyFabricFields, fctype, property_offset};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_GET;
    let f = PropertyFabricFields {
        attrib: 1,
        rsvd1: [0u8; 3],
        ofst: property_offset::CAP,
        value: 0,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    }
}
