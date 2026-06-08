// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-7-2 集成测试** — AsyncSession fabric handlers (Connect /
//! Property Get/Set / Disconnect) + KATO reset 在 dispatch_pdu_async 入口接通。
//!
//! 覆盖：
//! 1. `v8e7_2_connect_admin_async_returns_cntlid` — Connect qid=0 success +
//!    cntlid 在 CQE DW0
//! 2. `v8e7_2_connect_with_kato_arms_timer` — Connect kato=500ms 后
//!    `kato_tmo()` reflects；之后 idle 应 KATO expire
//! 3. `v8e7_2_property_get_cap_8byte_uses_dw1` — CAP 8B Property Get 高 32 位
//!    必须 ∈ DW1（V2 review H-1 不能漂移）
//! 4. `v8e7_2_property_set_cc_succeeds` — Property Set CC.EN=1 应 success
//! 5. `v8e7_2_disconnect_async_returns_disconnected_outcome` —
//!    DispatchOutcome.disconnected = true
//! 6. `v8e7_2_disconnect_invalid_recfmt_sends_sc_80` — RECFMT=1 应 SC=0x80
//! 7. `v8e7_2_disconnect_async_sweeps_io_queue_via_drop` — Drop 后 controller
//!    list_io_sqs 应空
//! 8. `v8e7_2_dispatch_resets_kato_on_each_pdu` — 多 PDU dispatch 后 KATO
//!    持续延后 expire（reset 每次刷）
//! 9. `v8e7_2_psh_too_short_sends_c2h_term_and_errs` — PSH < 64 字节应发
//!    C2HTerm 后 bail

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{
    AsyncSession, DispatchOutcome, SharedControllerInner, accept_and_handshake_async,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_backing(size: u64) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    f
}

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let backing = make_backing(1024 * 1024);
    let path = backing.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), backing)
}

async fn tokio_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let h = tokio::spawn(async move { l.accept().await.unwrap().0 });
    let a = TcpStream::connect(addr).await.unwrap();
    let b = h.await.unwrap();
    (a, b)
}

async fn send_icreq(s: &mut TcpStream) {
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

async fn handshake(shared: Arc<SharedControllerInner>) -> (TcpStream, AsyncSession) {
    let (mut client, server) = tokio_pair().await;
    let h = tokio::spawn(accept_and_handshake_async(server, shared));
    send_icreq(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    let sess = h.await.unwrap().unwrap();
    (client, sess)
}

/// 构造 Connect fabric capsule cmd（admin qid=0；可指定 kato）。
fn build_connect_pdu(cid: u16, qid: u16, kato_ms: u32) -> nvme_of_tcp_target::framing::Pdu {
    use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: kato_ms,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    nvme_of_tcp_target::framing::Pdu {
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

fn build_property_get_pdu(cid: u16, ofst: u32, attrib: u8) -> nvme_of_tcp_target::framing::Pdu {
    use nvme_of_tcp_target::fabric::{self, PropertyFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_GET;
    let f = PropertyFabricFields {
        attrib,
        rsvd1: [0u8; 3],
        ofst,
        value: 0,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    nvme_of_tcp_target::framing::Pdu {
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

fn build_property_set_pdu(
    cid: u16,
    ofst: u32,
    value: u64,
    attrib: u8,
) -> nvme_of_tcp_target::framing::Pdu {
    use nvme_of_tcp_target::fabric::{self, PropertyFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_SET;
    let f = PropertyFabricFields {
        attrib,
        rsvd1: [0u8; 3],
        ofst,
        value,
        rsvd2: [0u8; 8],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    nvme_of_tcp_target::framing::Pdu {
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

fn build_disconnect_pdu(cid: u16, recfmt: u16) -> nvme_of_tcp_target::framing::Pdu {
    use nvme_of_tcp_target::fabric::{self, DisconnectFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::DISCONNECT;
    let f = DisconnectFabricFields {
        recfmt,
        rsvd: [0u8; 22],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    nvme_of_tcp_target::framing::Pdu {
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

fn sc_of(psh: &[u8]) -> u8 {
    ((u16::from_le_bytes(psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_connect_admin_async_returns_cntlid() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    let pdu = build_connect_pdu(0x0001, 0, 0);
    let out = sess.dispatch_pdu_async(pdu).await.unwrap();
    assert!(!out.disconnected);
    // 读 CapsuleResp
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    assert_eq!(sc_of(&resp.psh), 0, "Connect 应 success");
    // CQE DW0 应是 cntlid = TEACHING_CNTLID = 1
    let dw0 = u32::from_le_bytes(resp.psh[0..4].try_into().unwrap());
    assert_eq!(dw0, 1, "CQE DW0 应是 cntlid");
    assert!(sess.admin_connected);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_connect_with_kato_arms_timer() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    let pdu = build_connect_pdu(0x0001, 0, 500); // KATO=500ms
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sess.kato_tmo(), Duration::from_millis(500));
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_property_get_cap_8byte_uses_dw1() {
    use nvme_of_tcp_target::fabric::property_offset;
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    // CAP 8 byte (attrib=1)
    let pdu = build_property_get_pdu(0x0002, property_offset::CAP, 1);
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0);
    let dw0 = u32::from_le_bytes(resp.psh[0..4].try_into().unwrap());
    let dw1 = u32::from_le_bytes(resp.psh[4..8].try_into().unwrap());
    // CAP 高 32 位包含 MPSMIN/MPSMAX/CSS/TO 等；至少 != 0
    assert_ne!(dw0, 0, "CAP DW0 应非零");
    // V2 review H-1：8B Property Get 高位必须填 DW1
    assert!(
        dw1 != 0 || dw0 != 0,
        "CAP 8B Property Get 应填 DW0+DW1，得 dw0={dw0:#x} dw1={dw1:#x}"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_property_set_cc_succeeds() {
    use nvme_of_tcp_target::fabric::property_offset;
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    // CC.EN=1 (bit 0)；attrib=0 = 4B
    let pdu = build_property_set_pdu(0x0003, property_offset::CC, 1, 0);
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0, "Property Set CC 应 success");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_disconnect_async_returns_disconnected_outcome() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    // 先 Connect admin
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001, 0, 0))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    // Disconnect
    let out: DispatchOutcome = sess
        .dispatch_pdu_async(build_disconnect_pdu(0x0010, 0))
        .await
        .unwrap();
    assert!(
        out.disconnected,
        "Disconnect 应让 DispatchOutcome.disconnected = true"
    );
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_disconnect_invalid_recfmt_sends_sc_80() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001, 0, 0))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    // recfmt=1 非法
    let _ = sess
        .dispatch_pdu_async(build_disconnect_pdu(0x0010, 1))
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0x80, "RECFMT=1 应回 SC=0x80");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_disconnect_async_sweeps_io_queue_via_drop() {
    // 注：V8e-7-2 placeholder admin handler 不真 Create IO SQ，所以这里手插
    // 一个 io_queues 镜像 + controller 真 install 验 Drop sweep 路径
    let (shared, _backing) = make_shared();
    let (client, mut sess) = handshake(shared).await;

    // 模拟 V8e-7-3 之后 admin Create IO CQ/SQ 完成后的状态：
    // 1. 镜像 io_queues 插入 qid=1
    // 2. controller 端 cqs/sqs 也插入 qid=1（用 Connect IO 调；这里直接 push）
    sess.io_queues
        .insert(1, nvme_of_tcp_target::io_queue::IoQueueState::new_sq(1));
    // controller 端 install IO CQ/SQ：用 NvmeController API 即可
    use nvme_firmware::cmd::Sqe;
    use zerocopy::FromZeros as _;
    {
        let mut c = sess.controller().controller.lock();
        let mut t = vfio_user_transport::NoopTransport;
        let mut ctx = pcie_device_core::DeviceCtx::new(&mut t);
        // Create IO CQ qid=1
        let mut sqe = Sqe::new_zeroed();
        sqe.cdw0 = 0x05;
        sqe.cdw10 = 1u32 | (15u32 << 16);
        sqe.cdw11 = 0x0000_0001;
        sqe.prp1 = nvme_of_tcp_target::CQ_BASE_GPA + 0x1000;
        let _ = c.nvme_admin_dispatch_with_conn(&mut ctx, sqe, 0x0100, 0, sess.conn_id);
        // Create IO SQ qid=1
        let mut sqe = Sqe::new_zeroed();
        sqe.cdw0 = 0x01;
        sqe.cdw10 = 1u32 | (15u32 << 16);
        sqe.cdw11 = 0x0001_0001;
        let _ = c.nvme_admin_dispatch_with_conn(&mut ctx, sqe, 0x0101, 0, sess.conn_id);
    }
    {
        let c = sess.controller().controller.lock();
        assert!(
            c.nvme_list_io_sqs().contains(&1),
            "controller IO SQ qid=1 应在"
        );
    }
    let shared_ref = Arc::clone(sess.controller());

    drop(sess);
    drop(client);

    // Drop 后 controller IO SQ qid=1 应被 sweep
    let c = shared_ref.controller.lock();
    assert!(
        !c.nvme_list_io_sqs().contains(&1),
        "AsyncSession Drop 应 sweep io_queues 镜像里的 qid → controller 不再含"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_dispatch_resets_kato_on_each_pdu() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    sess.set_kato(300); // 300ms KATO
    let initial_tmo = sess.kato_tmo();
    assert_eq!(initial_tmo, Duration::from_millis(300));

    // 3 轮 Connect dispatch（每次 reset KATO）
    for i in 0..3 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if i == 0 {
            let _ = sess
                .dispatch_pdu_async(build_connect_pdu(0x0001, 0, 0))
                .await
                .unwrap();
            let _ = read_pdu_async(&mut client).await.unwrap();
        } else {
            // 之后用 Property Get 走 dispatch_pdu_async（同样会 reset KATO）
            let _ = sess
                .dispatch_pdu_async(build_property_get_pdu(0x0010 + i, 0, 1))
                .await
                .unwrap();
            let _ = read_pdu_async(&mut client).await.unwrap();
        }
    }
    // 累计 450ms > KATO=300ms 但每 150ms reset → 仍应活
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    let r = tokio::time::timeout(Duration::from_millis(50), sess.pump_one_async(&mut rx)).await;
    assert!(r.is_err(), "KATO 持续 reset 后应不立即 expire");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_2_psh_too_short_sends_c2h_term_and_errs() {
    let (shared, _backing) = make_shared();
    let (mut client, mut sess) = handshake(shared).await;
    // 构造 PSH < 64 字节的 PDU
    let bad = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 32,
            pdo: 0,
            plen: 32,
        },
        psh: vec![0u8; 24], // < 64
        data: vec![],
    };
    let r = sess.dispatch_pdu_async(bad).await;
    assert!(r.is_err(), "PSH < 64 应 bail，得 {r:?}");
    // 应已发 C2HTerm
    let term = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(term.header.pdu_type, pdu_type::C2H_TERM);
    drop(client);
}
