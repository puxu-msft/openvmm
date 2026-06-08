// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V-followup-interop-6 wire capture** — 真启 in-process target + 纯 TCP
//! client 发 NVMe-oF Discovery 完整序列，**捕获 wire 字节**写到 /tmp 让人
//! 类 / Claude 用 hexdump 验证。
//!
//! 不需要 sudo / nvme-cli / kernel nvme-tcp。绕开 nvme-cli 自身解码的疑点
//! (上一轮我们改了 DiscoveryEntry layout 但 nvme-cli 输出"没变化" — 必须
//! 直接读 server 发出的真字节才能确认 wire 是否对)。
//!
//! 测试:
//! 1. v_interop_6_capture_discovery_log_wire — 完整流程 (ICReq → Connect
//!    discovery NQN → CC.EN → Identify CNS=1 → Get Log Page LID=0x70 8KB)，
//!    把 Discovery Log Page 4KB+ wire bytes 写到 /tmp/wire_discovery_log.bin
//! 2. v_interop_6_decode_discovery_log_at_anchored_offsets — 在测试内立刻
//!    解 wire bytes，对 entry 字段值断言 (subnqn, traddr, trsvcid)

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{
    self, ConnectData, ConnectFabricFields, PropertyFabricFields, fctype, property_offset,
};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_discovery_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(512).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let mut c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    // 注入 discovery portal 让 controller 切 discovery mode
    let portal = nvme_firmware::controller::discovery_log::DiscoveryPortal::from_ipv4_addr(
        "nqn.2014-08.org.nvmexpress:teaching:disk",
        "127.0.0.1:4420",
    )
    .unwrap();
    c.nvme_set_discovery_target(vec![portal]);
    (Arc::new(SharedControllerInner::new(c)), f)
}

fn build_icreq() -> (CommonHdr, Vec<u8>) {
    (
        CommonHdr {
            pdu_type: pdu_type::ICREQ,
            flags: 0,
            hlen: 128,
            pdo: 0,
            plen: 128,
        },
        IcPsh::default().as_bytes().to_vec(),
    )
}

fn build_connect(cid: u16, hostnqn: &str, subnqn: &str) -> Pdu {
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
    let mut cd = ConnectData::default();
    let n = hostnqn.len().min(cd.hostnqn.len());
    cd.hostnqn[..n].copy_from_slice(&hostnqn.as_bytes()[..n]);
    let n = subnqn.len().min(cd.subnqn.len());
    cd.subnqn[..n].copy_from_slice(&subnqn.as_bytes()[..n]);
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

fn build_property_set(cid: u16, ofst: u32, value: u64) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::PROPERTY_SET;
    let f = PropertyFabricFields {
        attrib: 0,
        rsvd1: [0u8; 3],
        ofst,
        value,
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

/// Identify Controller CNS=1
fn build_identify_ctrl(cid: u16) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x06;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x01u32.to_le_bytes()); // CNS=1
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

/// Get Log Page (admin opc 0x02)
///
/// cdw10: lid (bits 7:0) + lsp(bits 15:8) + rae(bit 15) + numdl(bits 31:16)
/// cdw11: numdu(bits 15:0) + lsi(bits 31:16)
/// cdw12: LPO low 32b
/// cdw13: LPO hi 32b
///
/// Spec § 5.16.1 — numdl = (bytes/4) - 1 (dword count 0-based)
fn build_get_log_page_discovery_with_lpo(cid: u16, bytes: u32, lpo: u64) -> Pdu {
    let dwords = bytes / 4;
    let numdl = (dwords - 1) & 0xffff;
    let lid: u32 = 0x70;
    let cdw10 = lid | (numdl << 16);
    let cdw12 = (lpo & 0xffff_ffff) as u32;
    let cdw13 = ((lpo >> 32) & 0xffff_ffff) as u32;
    let mut sqe = vec![0u8; 64];
    sqe[0] = 0x02;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
    sqe[48..52].copy_from_slice(&cdw12.to_le_bytes());
    sqe[52..56].copy_from_slice(&cdw13.to_le_bytes());
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

fn build_get_log_page_discovery(cid: u16, bytes: u32) -> Pdu {
    build_get_log_page_discovery_with_lpo(cid, bytes, 0)
}

/// **V-followup-interop-6** — 模拟 libnvme `nvme_discovery_log()` 真 2-phase
/// 流程 (而非我们之前的 8KB 单 request 假设)。
///
/// Phase 1: GLP len=20 LPO=0 → 拿 header NUMREC
/// Phase 2: GLP len=numrec*1024 **LPO=sizeof(header)=1024** → 拿 entries 部分
///
/// 上一轮 bug：admin.rs 忽略 LPO 直接调 build_discovery_log(bytes)，第 2 次
/// 拿到的还是 [0..bytes] header 截断，entries 没发，nvme-cli 看 NUMREC=1 但
/// entry 字段全空 (subnqn 空, trtype rdma=数字 1 误读)。
#[tokio::test(flavor = "multi_thread")]
async fn v_interop_6_libnvme_two_phase_discover_with_lpo() {
    use core::mem::offset_of;
    use nvme_firmware::controller::discovery_log::DiscoveryEntry;

    let (shared, _backing) = make_discovery_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> =
            accept_and_handshake_async(server, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..32 {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    if sess.dispatch_pdu_async(p).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    let mut c = TcpStream::connect(addr).await.unwrap();
    let (h, psh) = build_icreq();
    write_pdu_async(&mut c, &h, &psh, &[]).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    let connect = build_connect(
        0x0001,
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress.discovery",
    );
    write_pdu_async(&mut c, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    let ps = build_property_set(0x0002, property_offset::CC, 0x0046_0001);
    write_pdu_async(&mut c, &ps.header, &ps.psh, &ps.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();

    // ===== Phase 1: probe header =====
    let glp1 = build_get_log_page_discovery_with_lpo(0x0010, 20, 0);
    write_pdu_async(&mut c, &glp1.header, &glp1.psh, &glp1.data)
        .await
        .unwrap();
    let data1 = read_pdu_async(&mut c).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(data1.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data1.data.len(), 20);
    let numrec = u64::from_le_bytes(data1.data[8..16].try_into().unwrap());
    assert_eq!(numrec, 1, "phase 1: NUMREC=1");

    // ===== Phase 2: pull entries (LPO=1024) =====
    let entries_size = (numrec as u32) * 1024;
    let glp2 = build_get_log_page_discovery_with_lpo(0x0011, entries_size, 1024);
    write_pdu_async(&mut c, &glp2.header, &glp2.psh, &glp2.data)
        .await
        .unwrap();
    let data2 = read_pdu_async(&mut c).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(data2.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data2.data.len() as u32, entries_size);

    // entries 应直接是 entry[0] (因为 LPO 跳过 header)
    let entry = &data2.data[..1024];
    let trtype = entry[offset_of!(DiscoveryEntry, trtype)];
    let subtype = entry[offset_of!(DiscoveryEntry, subtype)];
    assert_eq!(
        trtype, 3,
        "phase 2: entry[0] TRTYPE=3 (TCP) — 之前 bug 时此字节是 0 (假 RDMA)"
    );
    assert_eq!(subtype, 2, "phase 2: entry[0] SUBTYPE=2 (NVM Subsystem)");
    let o = offset_of!(DiscoveryEntry, subnqn);
    let end = entry[o..o + 256]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(256);
    let subnqn = std::str::from_utf8(&entry[o..o + end]).unwrap();
    assert_eq!(subnqn, "nqn.2014-08.org.nvmexpress:teaching:disk");

    c.shutdown().await.unwrap();
    let _ = server.await;
}

fn cqe_sc(pdu: &Pdu) -> u8 {
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP, "expected CapsuleResp");
    let status = u16::from_le_bytes([pdu.psh[14], pdu.psh[15]]);
    ((status >> 1) & 0xFF) as u8
}

#[tokio::test(flavor = "multi_thread")]
async fn v_interop_6_capture_discovery_log_wire() {
    let (shared, _backing) = make_discovery_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> =
            accept_and_handshake_async(server, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..16 {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    if sess.dispatch_pdu_async(p).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    let mut c = TcpStream::connect(addr).await.unwrap();

    // 1. ICReq → ICResp
    let (h, psh) = build_icreq();
    write_pdu_async(&mut c, &h, &psh, &[]).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();

    // 2. Connect discovery NQN
    let connect = build_connect(
        0x0001,
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress.discovery",
    );
    write_pdu_async(&mut c, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(cqe_sc(&r), 0, "Connect discovery SC=0");

    // 3. Property Set CC = 0x460001 (CC.EN=1)
    let ps = build_property_set(0x0002, property_offset::CC, 0x0046_0001);
    write_pdu_async(&mut c, &ps.header, &ps.psh, &ps.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(cqe_sc(&r), 0);

    // 4. Identify Controller (CNS=1)
    let id = build_identify_ctrl(0x0003);
    write_pdu_async(&mut c, &id.header, &id.psh, &id.data)
        .await
        .unwrap();
    // 收 C2HData(4 KiB) + CapsuleResp
    let id_data = read_pdu_async(&mut c).await.unwrap();
    let id_rsp = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(id_data.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(id_data.data.len(), 4096);
    assert_eq!(cqe_sc(&id_rsp), 0);

    // 把 Identify Controller 4 KiB 落盘
    std::fs::write("/tmp/wire_identify_ctrl.bin", &id_data.data).unwrap();

    // 5. Get Log Page LID=0x70 (Discovery Log) 8 KiB
    let glp = build_get_log_page_discovery(0x0004, 8192);
    write_pdu_async(&mut c, &glp.header, &glp.psh, &glp.data)
        .await
        .unwrap();
    let log_data = read_pdu_async(&mut c).await.unwrap();
    let log_rsp = read_pdu_async(&mut c).await.unwrap();
    assert_eq!(
        log_data.header.pdu_type,
        pdu_type::C2H_DATA,
        "expected C2HData for Discovery Log"
    );
    assert_eq!(cqe_sc(&log_rsp), 0, "Discovery Log Page SC=0");

    // 落 Discovery Log Page wire bytes
    std::fs::write("/tmp/wire_discovery_log.bin", &log_data.data).unwrap();
    eprintln!(
        "captured Discovery Log Page = {} bytes -> /tmp/wire_discovery_log.bin",
        log_data.data.len()
    );

    c.shutdown().await.unwrap();
    let _ = server.await;
}

/// 上一个测试 capture 完后，本测试直接读 /tmp/wire_discovery_log.bin
/// (cargo test 跑顺序：alphabetical → `capture_...` 在 `decode_...` 之前)
/// 用 spec offset 解 entry 字段，断言 nvme-cli **应该**能解出的值。
#[tokio::test(flavor = "multi_thread")]
async fn v_interop_6_decode_discovery_log_at_anchored_offsets() {
    use core::mem::offset_of;
    use nvme_firmware::controller::discovery_log::DiscoveryEntry;

    // 触发 capture (本 test 自包含；不依赖测试运行顺序)
    let (shared, _backing) = make_discovery_shared();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = Arc::clone(&shared);
    let server = tokio::spawn(async move {
        let (server, _) = listener.accept().await.unwrap();
        let mut sess: AsyncSession<TcpStream> =
            accept_and_handshake_async(server, s).await.unwrap();
        let (_tx, mut rx) = tokio::sync::watch::channel(false);
        for _ in 0..16 {
            match sess.pump_one_async(&mut rx).await {
                Ok(nvme_of_tcp_target::PumpEvent::Pdu(p)) => {
                    if sess.dispatch_pdu_async(p).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    let mut c = TcpStream::connect(addr).await.unwrap();
    let (h, psh) = build_icreq();
    write_pdu_async(&mut c, &h, &psh, &[]).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    let connect = build_connect(
        0x0001,
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress.discovery",
    );
    write_pdu_async(&mut c, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    let ps = build_property_set(0x0002, property_offset::CC, 0x0046_0001);
    write_pdu_async(&mut c, &ps.header, &ps.psh, &ps.data)
        .await
        .unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    let glp = build_get_log_page_discovery(0x0003, 8192);
    write_pdu_async(&mut c, &glp.header, &glp.psh, &glp.data)
        .await
        .unwrap();
    let log_data = read_pdu_async(&mut c).await.unwrap();
    let _ = read_pdu_async(&mut c).await.unwrap();
    c.shutdown().await.unwrap();
    let _ = server.await;

    // ====== 真 wire bytes 解析 ======
    let wire = &log_data.data;
    assert!(wire.len() >= 2048, "至少 1 KiB header + 1 KiB entry");

    // Header @ offset 0..16
    let gen_ctr = u64::from_le_bytes(wire[0..8].try_into().unwrap());
    let numrec = u64::from_le_bytes(wire[8..16].try_into().unwrap());
    let recfmt = u16::from_le_bytes([wire[16], wire[17]]);
    eprintln!("GENCTR={gen_ctr} NUMREC={numrec} RECFMT={recfmt}");
    assert_eq!(numrec, 1, "我们注入 1 portal");
    assert_eq!(recfmt, 0);

    // Entry[0] @ offset 1024..2048
    let entry = &wire[1024..2048];
    let trtype = entry[offset_of!(DiscoveryEntry, trtype)];
    let adrfam = entry[offset_of!(DiscoveryEntry, adrfam)];
    let subtype = entry[offset_of!(DiscoveryEntry, subtype)];
    eprintln!("Entry[0]: trtype={trtype} adrfam={adrfam} subtype={subtype}");
    assert_eq!(trtype, 3, "TRTYPE=3 (TCP) — 若 = 1 host 解为 RDMA");
    assert_eq!(adrfam, 1, "ADRFAM=1 (IPv4)");
    assert_eq!(subtype, 2, "SUBTYPE=2 (NVM Subsystem)");

    let o = offset_of!(DiscoveryEntry, trsvcid);
    let end = entry[o..o + 32].iter().position(|&b| b == 0).unwrap_or(32);
    let trsvcid = std::str::from_utf8(&entry[o..o + end]).unwrap();
    eprintln!("TRSVCID @ {o}..{}: {trsvcid:?}", o + end);
    assert_eq!(trsvcid, "4420");

    let o = offset_of!(DiscoveryEntry, subnqn);
    let end = entry[o..o + 256]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(256);
    let subnqn = std::str::from_utf8(&entry[o..o + end]).unwrap();
    eprintln!("SUBNQN @ {o}..{}: {subnqn:?}", o + end);
    assert_eq!(subnqn, "nqn.2014-08.org.nvmexpress:teaching:disk");

    let o = offset_of!(DiscoveryEntry, traddr);
    let end = entry[o..o + 256]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(256);
    let traddr = std::str::from_utf8(&entry[o..o + end]).unwrap();
    eprintln!("TRADDR @ {o}..{}: {traddr:?}", o + end);
    assert_eq!(traddr, "127.0.0.1");
}
