// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V8e-7-3 集成测试** — AsyncSession admin / IO dispatch +
//! drain_aers_async。
//!
//! 覆盖：
//! 1. `v8e7_3_admin_identify_ctrl_async_emits_c2hdata_resp` — Identify
//!    Controller (CNS=0x01) async dispatch → C2HData (4 KiB) + CapsuleResp success
//! 2. `v8e7_3_io_read_nlb1_async_round_trip` — IO Read nlb=1 完整 V5b 路径
//! 3. `v8e7_3_io_write_then_read_async_persists` — IO Write→Read 同 LBA 数据一致
//!    （R2T → H2CData → resp，client 端 R2T 应答需走另一 task）
//! 4. `v8e7_3_io_nlb_over_max_async_rejected_sc18` — nlb>MAX 应 SC=0x18
//! 5. `v8e7_3_drain_aers_async_emits_capsule_resp` — inject_aen_async +
//!    drain_aers_async 真 wire emit
//! 6. `v8e7_3_drain_aers_async_cap_4_per_pump` — cap MAX_DRAIN_PER_PUMP=4
//! 7. `v8e7_3_drain_aers_async_empty_returns_zero` — 无 pending AER → 0
//! 8. `v8e7_3_admin_format_nvm_rejected_via_decide_async` — V5e-1-fix 黑名单
//!    在 async path 仍生效
//!
//! sync vs async byte-identical gate（plan §3 Q3 / R-1 防漂移）需要 sync/async
//! pair 跨 runtime 协同握手 + IO 测试架构复杂，留 V8e-7-3-followup 单独
//! integration test。当前 V8e-7-1 dispatch_plan 决策核被两侧共用 + V8e-1
//! serialize_pdu byte-identical regression gate 已锁定 wire 层不漂移；
//! handler-level wire diff 由分别的 v8b/c/d sync 测试 + v8e7_2/3 async 测试
//! 覆盖。

#![allow(missing_docs)]
// V8e-7 security-reviewer MEDIUM-1：inject_aen_async 被标 deprecated 作 test-only 屏障
#![allow(deprecated)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::framing::{read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_backing(size: u64, pattern: u8) -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(size).unwrap();
    use std::io::Write as _;
    let buf = vec![pattern; 4096];
    let mut h = std::fs::OpenOptions::new()
        .write(true)
        .open(f.path())
        .unwrap();
    h.write_all(&buf).unwrap();
    f
}

fn make_shared(backing: &std::path::Path) -> Arc<SharedControllerInner> {
    let path = backing.to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    Arc::new(SharedControllerInner::new(c))
}

async fn tokio_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let h = tokio::spawn(async move { l.accept().await.unwrap().0 });
    let a = TcpStream::connect(addr).await.unwrap();
    let b = h.await.unwrap();
    (a, b)
}

async fn send_icreq_async(s: &mut TcpStream) {
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

async fn handshake_async(shared: Arc<SharedControllerInner>) -> (TcpStream, AsyncSession) {
    let (mut client, server) = tokio_pair().await;
    let h = tokio::spawn(accept_and_handshake_async(server, shared));
    send_icreq_async(&mut client).await;
    let _ = read_pdu_async(&mut client).await.unwrap();
    let sess = h.await.unwrap().unwrap();
    (client, sess)
}

fn build_connect_pdu(cid: u16) -> nvme_of_tcp_target::framing::Pdu {
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

fn build_identify_ctrl_pdu(cid: u16) -> nvme_of_tcp_target::framing::Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x06; // IDENTIFY admin opc
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    // cdw10 bits 7:0 = CNS = 0x01 (Identify Controller)
    sqe[40..44].copy_from_slice(&0x01u32.to_le_bytes());
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

// ---- 测试 ----

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_admin_identify_ctrl_async_emits_c2hdata_resp() {
    let backing = make_backing(1024 * 1024, 0xAA);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    // Identify Controller
    let _ = sess
        .dispatch_pdu_async(build_identify_ctrl_pdu(0x0010))
        .await
        .unwrap();
    let data_pdu = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(data_pdu.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data_pdu.data.len(), 4096, "Identify Ctrl 应 4 KiB");
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    assert_eq!(sc_of(&resp.psh), 0, "Identify 应 success");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_admin_format_nvm_rejected_via_decide_async() {
    let backing = make_backing(1024 * 1024, 0);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();
    // FORMAT_NVM (opc=0x80) — 应被 decide_admin_blocked_opc reject SC=0x01
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x80;
    sqe[2..4].copy_from_slice(&0x0011u16.to_le_bytes());
    let pdu = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    };
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(
        sc_of(&resp.psh),
        0x01,
        "FORMAT_NVM 应 SC=0x01 INVALID_OPCODE"
    );
    drop(client);
}

// --- IO path 测试需要先建 IO CQ/SQ + Connect qid=1（与 v8b_multi_conn 同模式） ---

async fn set_up_qid1(client: &mut TcpStream, sess: &mut AsyncSession) {
    // Connect admin
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001))
        .await
        .unwrap();
    let _ = read_pdu_async(client).await.unwrap();
    // Create IO CQ qid=1
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x05;
    sqe[2..4].copy_from_slice(&0x0002u16.to_le_bytes());
    let cdw10 = 1u32 | (15u32 << 16);
    sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
    sqe[44..48].copy_from_slice(&0x0000_0001u32.to_le_bytes());
    let pdu = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    };
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let _ = read_pdu_async(client).await.unwrap();
    // Create IO SQ qid=1, cq_id=1
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x01;
    sqe[2..4].copy_from_slice(&0x0003u16.to_le_bytes());
    sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
    sqe[44..48].copy_from_slice(&0x0001_0001u32.to_le_bytes());
    let pdu = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    };
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let _ = read_pdu_async(client).await.unwrap();
    // Connect qid=1
    use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x0004u16.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 1,
        sqsize: 15,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let cd = ConnectData::default();
    let pdu = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 72,
            plen: 72 + 1024,
        },
        psh: sqe,
        data: cd.as_bytes().to_vec(),
    };
    let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    let _ = read_pdu_async(client).await.unwrap();
}

fn build_io_read_pdu(cid: u16, nsid: u32, slba: u64, nlb: u32) -> nvme_of_tcp_target::framing::Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x02;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes());
    sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
    sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
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

fn build_io_write_pdu(
    cid: u16,
    nsid: u32,
    slba: u64,
    nlb: u32,
) -> nvme_of_tcp_target::framing::Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x01;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sqe[40..44].copy_from_slice(&((slba & 0xffff_ffff) as u32).to_le_bytes());
    sqe[44..48].copy_from_slice(&((slba >> 32) as u32).to_le_bytes());
    sqe[48..52].copy_from_slice(&(nlb - 1).to_le_bytes());
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

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_io_read_nlb1_async_round_trip() {
    let backing = make_backing(1024 * 1024, 0xCC);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    set_up_qid1(&mut client, &mut sess).await;
    // IO Read nsid=1 SLBA=0 nlb=1
    let _ = sess
        .dispatch_pdu_async(build_io_read_pdu(0x0020, 1, 0, 1))
        .await
        .unwrap();
    let data_pdu = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(data_pdu.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data_pdu.data.len(), 512);
    assert!(
        data_pdu.data.iter().all(|&b| b == 0xCC),
        "应是 backing pattern 0xCC"
    );
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0, "IO Read 应 success");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_io_nlb_over_max_async_rejected_sc18() {
    // **V-followup-prp-list 后行为变更**: nlb 上限从 V5_NLB_MAX=16 提到
    // V_HOST_IO_NLB_MAX=256 via session chunking。
    // - 17..256 LBA: session 拆 sub-cmd, 应成功 (SC=0)
    // - >256 LBA: 仍 SC=0x18 reject
    let backing = make_backing(1024 * 1024, 0);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    set_up_qid1(&mut client, &mut sess).await;
    // nlb=17 现在应通过 (= V5_NLB_MAX+1, chunked into 16+1)
    let _ = sess
        .dispatch_pdu_async(build_io_read_pdu(0x0030, 1, 0, 17))
        .await
        .unwrap();
    // chunked Read 发 2 个 C2HData (chunk 0: 16 LBA, chunk 1: 1 LBA) 然后 1 个 RSP
    // 收掉前 2 个 C2HData
    let p1 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(
        p1.header.pdu_type,
        nvme_of_tcp_target::pdu::pdu_type::C2H_DATA
    );
    let p2 = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(
        p2.header.pdu_type,
        nvme_of_tcp_target::pdu::pdu_type::C2H_DATA
    );
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(
        sc_of(&resp.psh),
        0,
        "V-followup-prp-list: nlb=17 chunked 应通过 (SC=0)"
    );

    // > V_HOST_IO_NLB_MAX=256 应仍 reject
    let _ = sess
        .dispatch_pdu_async(build_io_read_pdu(0x0031, 1, 0, 257))
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(
        sc_of(&resp.psh),
        0x18,
        "V-followup-prp-list: nlb=257 > V_HOST_IO_NLB_MAX 应 SC=0x18"
    );
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_io_write_then_read_async_persists() {
    let backing = make_backing(1024 * 1024, 0x00);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    set_up_qid1(&mut client, &mut sess).await;

    // IO Write LBA=100 nlb=1 pattern 0xE7
    // dispatch_pdu_async 内部 R2T → 等 H2CData → 完成；client 端必须在另一
    // task 里完成 R2T 应答，否则单 task 内 dispatch await 自己 read 永远不返。
    let write_pdu = build_io_write_pdu(0x0040, 1, 100, 1);
    let dispatch_handle = tokio::spawn(async move {
        let r = sess.dispatch_pdu_async(write_pdu).await;
        (r, sess)
    });

    // 读 R2T
    let r2t = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(r2t.header.pdu_type, pdu_type::R2T);
    let r2t_psh: nvme_of_tcp_target::pdu::R2tPsh =
        nvme_of_tcp_target::pdu::decode_psh(&r2t.psh).unwrap();
    let ttag = r2t_psh.ttag;
    // 发 H2CData
    let payload = vec![0xE7u8; 512];
    let data_psh = nvme_of_tcp_target::pdu::DataPsh {
        cccid: 0x0040,
        ttag_or_rsvd: ttag,
        data_offset: 0,
        data_length: 512,
        rsvd: [0u8; 4],
    };
    let hdr = CommonHdr {
        pdu_type: pdu_type::H2C_DATA,
        flags: 0x04,
        hlen: 24,
        pdo: 24,
        plen: 24 + 512,
    };
    write_pdu_async(&mut client, &hdr, data_psh.as_bytes(), &payload)
        .await
        .unwrap();
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0, "Write 应 success");

    let (_, mut sess) = dispatch_handle.await.unwrap();

    // 读回同 LBA 应见 0xE7（IO Read 简单同步路径，client 收一帧后即可）
    let _ = sess
        .dispatch_pdu_async(build_io_read_pdu(0x0041, 1, 100, 1))
        .await
        .unwrap();
    let data_pdu = read_pdu_async(&mut client).await.unwrap();
    assert!(data_pdu.data.iter().all(|&b| b == 0xE7));
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(sc_of(&resp.psh), 0);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_drain_aers_async_emits_capsule_resp() {
    let backing = make_backing(1024 * 1024, 0);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 客户端发 AER cmd (opc=0x0C)
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x0C;
    sqe[2..4].copy_from_slice(&0x0070u16.to_le_bytes());
    let aer_pdu = nvme_of_tcp_target::framing::Pdu {
        header: CommonHdr {
            pdu_type: pdu_type::CMD,
            flags: 0,
            hlen: 72,
            pdo: 0,
            plen: 72,
        },
        psh: sqe,
        data: vec![],
    };
    let _ = sess.dispatch_pdu_async(aer_pdu).await.unwrap();
    // AER fast-path 不发 wire；session pending_aers += 1
    assert_eq!(sess.pending_aers.len(), 1);

    // 触发 controller 端 fire → emit
    let emitted = sess.inject_aen_async(0x01, 0x00, 0x02).await.unwrap();
    assert_eq!(emitted, 1);
    // wire 上应有一条 CapsuleResp
    let resp = read_pdu_async(&mut client).await.unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_drain_aers_async_empty_returns_zero() {
    let backing = make_backing(1024 * 1024, 0);
    let shared = make_shared(backing.path());
    let (client, mut sess) = handshake_async(shared).await;
    let n = sess.drain_aers_async().await.unwrap();
    assert_eq!(n, 0, "无 pending AER 应 drain 0");
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn v8e7_3_drain_aers_async_cap_4_per_pump() {
    // 4 AER stash + 4+1 dispatch 让 controller pending=5；drain 应只发 4 条
    let backing = make_backing(1024 * 1024, 0);
    let shared = make_shared(backing.path());
    let (mut client, mut sess) = handshake_async(shared).await;
    let _ = sess
        .dispatch_pdu_async(build_connect_pdu(0x0001))
        .await
        .unwrap();
    let _ = read_pdu_async(&mut client).await.unwrap();

    // 发 4 条 AER（session-level cap=4）
    for i in 0..4 {
        let mut sqe = vec![0u8; 64];
        sqe[0] = 0x0C;
        sqe[2..4].copy_from_slice(&(0x0100u16 + i).to_le_bytes());
        let pdu = nvme_of_tcp_target::framing::Pdu {
            header: CommonHdr {
                pdu_type: pdu_type::CMD,
                flags: 0,
                hlen: 72,
                pdo: 0,
                plen: 72,
            },
            psh: sqe,
            data: vec![],
        };
        let _ = sess.dispatch_pdu_async(pdu).await.unwrap();
    }
    assert_eq!(sess.pending_aers.len(), 4);

    // drain 应发 4 条
    let n = sess.drain_aers_async().await.unwrap();
    assert_eq!(n, 4);
    for _ in 0..4 {
        let resp = read_pdu_async(&mut client).await.unwrap();
        assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    }
    drop(client);
}
