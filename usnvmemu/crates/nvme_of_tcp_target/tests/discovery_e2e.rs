// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V7** — Discovery subsystem 端到端 integration test。
//!
//! 验证：
//! - controller `nvme_set_discovery_target` 注入 portal
//! - session `discovery_mode` derive
//! - Connect Data subnqn 必须 = DISCOVERY_NQN
//! - admin Get Log Page LID=0x70 返 Discovery Log Page byte-exact
//! - admin opc 白名单（非 Discovery cmd → INVALID_OPCODE）

use nvme_firmware::NvmeController;
use nvme_firmware::controller::discovery_log::DiscoveryPortal;
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{DISCOVERY_NQN, V2Session};
use std::net::{TcpListener, TcpStream};
use std::thread;
use zerocopy::IntoBytes;

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let t = thread::spawn(move || listener.accept().unwrap().0);
    let client = TcpStream::connect(addr).unwrap();
    let server = t.join().unwrap();
    (client, server)
}

fn make_discovery_controller() -> (NvmeController, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().expect("tempfile");
    f.as_file().set_len(1024 * 1024).expect("set_len");
    let path = f.path().to_str().expect("utf8").to_string();
    let mut c = NvmeController::open(&[path], 0x1414, 0, &[]).expect("open controller");
    let portal =
        DiscoveryPortal::from_ipv4_addr("nqn.2026-06.io.openhcl:nvme.userspace", "127.0.0.1:4421")
            .unwrap();
    c.nvme_set_discovery_target(vec![portal]);
    (c, f)
}

fn send_icreq(client: &mut TcpStream) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    let psh = IcPsh {
        pfv: 0,
        hpda_or_cpda: 0,
        digest: 0,
        maxr2t_or_maxh2cdata: 7,
        rsvd: [0u8; 112],
    };
    write_pdu(client, &hdr, psh.as_bytes(), &[]).unwrap();
}

fn send_connect_with_subnqn(client: &mut TcpStream, cid: u16, subnqn: &str) {
    let mut sqe = [0u8; 64];
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
    let n = subnqn.len().min(256);
    cd.subnqn[..n].copy_from_slice(&subnqn.as_bytes()[..n]);
    let cmd_hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(client, &cmd_hdr, &sqe, cd.as_bytes()).unwrap();
}

fn send_get_log_page(client: &mut TcpStream, cid: u16, lid: u8, bytes: u32) {
    // NUMD = bytes/4 - 1 (zero-based dwords); 取 lo 16 bits 放 cdw10[31:16]，
    // hi 16 bits 放 cdw11[15:0]
    let numd_zero = bytes / 4 - 1;
    let numd_lo = numd_zero & 0xFFFF;
    let numd_hi = (numd_zero >> 16) & 0xFFFF;
    let cdw10 = (lid as u32) | (numd_lo << 16);
    let cdw11 = numd_hi;
    let mut sqe = [0u8; 64];
    sqe[0] = 0x02; // admin_opc::GET_LOG_PAGE
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[40..44].copy_from_slice(&cdw10.to_le_bytes());
    sqe[44..48].copy_from_slice(&cdw11.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(client, &hdr, &sqe, &[]).unwrap();
}

/// **V7-e2e-1** — Discovery Connect → Get Log 0x70 (header) → 验
/// NUMREC=1 与 portal NQN byte-exact。
#[test]
fn v7_e2e_discover_one_portal_round_trip() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_discovery_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect (discovery NQN)
        sess.pump_one()?; // Get Log Page 0x70 header
        sess.pump_one()?; // Get Log Page 0x70 full
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();

    // Connect 用 Discovery NQN → 应 success
    send_connect_with_subnqn(&mut client, 0x0001, DISCOVERY_NQN);
    let resp = read_pdu(&mut client).unwrap();
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0, "Discovery Connect 应 success");

    // Get Log Page 0x70 仅取 header (1024B) 探 NUMREC
    send_get_log_page(&mut client, 0x0070, 0x70, 1024);
    let data_pdu = read_pdu(&mut client).unwrap();
    assert_eq!(data_pdu.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data_pdu.data.len(), 1024);
    let numrec = u64::from_le_bytes(data_pdu.data[8..16].try_into().unwrap());
    assert_eq!(numrec, 1, "应有 1 个 portal entry");
    let resp = read_pdu(&mut client).unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0);

    // Get Log Page 0x70 full (header + 1 entry = 2048B)
    send_get_log_page(&mut client, 0x0071, 0x70, 2048);
    let data_pdu = read_pdu(&mut client).unwrap();
    assert_eq!(data_pdu.data.len(), 2048);
    // entry[0] @ offset 1024
    let entry = &data_pdu.data[1024..2048];
    assert_eq!(entry[0], 3, "TRTYPE=3 (TCP)");
    assert_eq!(entry[1], 1, "ADRFAM=1 (IPv4)");
    assert_eq!(entry[2], 2, "SUBTYPE=2 (NVM)");
    // SUBNQN at offset 0x100
    assert_eq!(
        &entry[0x100..0x100 + "nqn.2026-06.io.openhcl:nvme.userspace".len()],
        b"nqn.2026-06.io.openhcl:nvme.userspace"
    );
    // TRADDR at offset 0x200
    assert_eq!(&entry[0x200..0x200 + "127.0.0.1".len()], b"127.0.0.1");
    // TRSVCID at offset 0x20
    assert_eq!(&entry[0x20..0x20 + "4421".len()], b"4421");
    let resp = read_pdu(&mut client).unwrap();
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0);

    h.join().unwrap().unwrap();
}

/// **V7-e2e-2** — Discovery mode Connect 用 *非* Discovery NQN → reject
#[test]
fn v7_e2e_discovery_rejects_wrong_nqn() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_discovery_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect (错 NQN)
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();

    // 用 NVM subsystem NQN 而非 Discovery NQN
    send_connect_with_subnqn(&mut client, 0x0001, "nqn.2026-06.io.openhcl:nvme.userspace");
    let resp = read_pdu(&mut client).unwrap();
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(
        sc, 0x82,
        "Discovery mode 下 subnqn != DISCOVERY_NQN 必返 CONNECT_INVALID_PARAM (0x82)"
    );
    h.join().unwrap().unwrap();
}

/// **V7-e2e-3** — Discovery mode 下非白名单 admin opc (Write 0x01) →
/// INVALID_OPCODE
#[test]
fn v7_e2e_discovery_rejects_non_whitelisted_admin_opc() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_discovery_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect
        sess.pump_one()?; // Write (0x01) on admin queue → reject
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();
    send_connect_with_subnqn(&mut client, 0x0001, DISCOVERY_NQN);
    let _ = read_pdu(&mut client).unwrap();

    // 假装发 Write opc=0x01 在 admin queue 上（current_qid=0）
    let mut sqe = [0u8; 64];
    sqe[0] = 0x01;
    sqe[2..4].copy_from_slice(&0xDEADu16.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(&mut client, &hdr, &sqe, &[]).unwrap();
    let resp = read_pdu(&mut client).unwrap();
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0x01, "非白名单 opc 应返 INVALID_OPCODE");
    h.join().unwrap().unwrap();
}

/// **V7c-fix (review H-1)** — Discovery mode 下 Identify Controller (CNS=0x01)
/// 必须报 CNTRLTYPE=0x02 + NN=0；Linux nvme-cli driver 据此 fingerprint
/// 走 Discovery 路径而非 IO 路径。
#[test]
fn v7c_e2e_identify_controller_cntrltype_discovery() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_discovery_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect
        sess.pump_one()?; // Identify Controller
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();
    send_connect_with_subnqn(&mut client, 0x0001, DISCOVERY_NQN);
    let _ = read_pdu(&mut client).unwrap();

    // Identify Controller (opc=0x06, CNS=1)
    let mut sqe = [0u8; 64];
    sqe[0] = 0x06;
    sqe[2..4].copy_from_slice(&0x00C1u16.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // CNS=1
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(&mut client, &hdr, &sqe, &[]).unwrap();

    let data_pdu = read_pdu(&mut client).unwrap();
    assert_eq!(data_pdu.header.pdu_type, pdu_type::C2H_DATA);
    assert_eq!(data_pdu.data.len(), 4096);
    // byte 111 = CNTRLTYPE (spec § 5.17.2.1 Figure 312)
    assert_eq!(
        data_pdu.data[111], 0x02,
        "Discovery mode CNTRLTYPE 必须 = 0x02 (review H-1)"
    );
    // NN at offset 516..520 = 0
    let nn = u32::from_le_bytes(data_pdu.data[516..520].try_into().unwrap());
    assert_eq!(nn, 0, "Discovery mode NN 必须 = 0");
    let resp = read_pdu(&mut client).unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0);
    h.join().unwrap().unwrap();
}

/// **V7c-fix (review H-2)** — Discovery mode 下 IO Connect (qid≥1)
/// 必显式 reject CONNECT_INVALID_PARAM (0x82)。
#[test]
fn v7c_e2e_discovery_rejects_io_connect_qid() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_discovery_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect admin
        sess.pump_one()?; // Connect qid=1 (应被拒)
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();
    send_connect_with_subnqn(&mut client, 0x0001, DISCOVERY_NQN);
    let _ = read_pdu(&mut client).unwrap();

    // 构造 Connect qid=1（discovery mode 应直接拒，不经过 io_queues check）
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x0002u16.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid: 1,
        sqsize: 31,
        cattr: 0,
        rsvd1: 0,
        kato: 0,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let mut cd = ConnectData::default();
    let want = DISCOVERY_NQN;
    cd.subnqn[..want.len()].copy_from_slice(want.as_bytes());
    let cmd_hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(&mut client, &cmd_hdr, &sqe, cd.as_bytes()).unwrap();

    let resp = read_pdu(&mut client).unwrap();
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(
        sc, 0x82,
        "Discovery mode 下 IO Connect (qid≥1) 必返 CONNECT_INVALID_PARAM"
    );
    h.join().unwrap().unwrap();
}
