// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V6c** — AER 端到端 integration tests
//!
//! 与 src/session.rs 内的 unit tests 不同：本 file 跑完整 wire path
//! 模拟 Linux nvme-cli 的 AER 行为序列：
//! - host 发 4 个 AER（MAX_PENDING_AERS 上限）
//! - session 通过 [`V2Session::inject_aen`] 显式触发 2 条 AEN event
//! - wire 上严格按 fire 顺序出 2 条 CapsuleResp，每条 cdw0 编码 type/info/log_id
//! - 剩 2 条 AER 仍 pending，第 5 条 inject 应被 controller drop（fire_aen 返 false）
//!
//! e2e 覆盖：
//! - V6a fast-path（AER 不 bail）
//! - V6b inject_aen wire emit
//! - 多事件超 pending 时 spec 允许的 drop 语义

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{read_pdu, write_pdu};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{V2Session, aer};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use zerocopy::IntoBytes;

fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let t = thread::spawn(move || listener.accept().unwrap().0);
    let client = TcpStream::connect(addr).unwrap();
    let server = t.join().unwrap();
    (client, server)
}

fn make_controller() -> (NvmeController, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().expect("tempfile");
    f.as_file().set_len(1024 * 1024).expect("set_len");
    let path = f.path().to_str().expect("utf8").to_string();
    let c = NvmeController::open(&[path], 0x1414, 0, &[]).expect("open controller");
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

fn send_connect_admin(client: &mut TcpStream) {
    let mut sqe = [0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&0x0001u16.to_le_bytes());
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
    let cmd_hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 72,
        plen: 72 + 1024,
    };
    write_pdu(client, &cmd_hdr, &sqe, cd.as_bytes()).unwrap();
}

fn send_aer(client: &mut TcpStream, cid: u16) {
    let mut sqe = [0u8; 64];
    sqe[0] = aer::ADMIN_OPC_AER;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(client, &hdr, &sqe, &[]).unwrap();
}

/// **V6c-1** — host 发 4 个 AER 填满；session inject 2 条 AEN；wire 严格按
/// FIFO 出 2 条 CapsuleResp；剩 2 条 AER 仍 pending；第 3 次 inject 应 fire
/// 第 3 条 AER；第 5 次 inject（仅剩 1 AER）后再 inject 第 6 应返 0（drop）。
#[test]
fn v6c_e2e_4_aers_then_3_injects_drops_after_4th() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_controller();

    let (inject_tx, inject_rx) = mpsc::channel::<(u8, u8, u8)>();
    let (count_tx, count_rx) = mpsc::channel::<usize>();

    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect admin
        // 4 个 AER
        for _ in 0..4 {
            sess.pump_one()?;
        }
        // 等 test 主线程 inject 信号；每收到一个 (type, info, log) inject 并报 emit count
        for (t, i, l) in inject_rx.iter() {
            let n = sess.inject_aen(t, i, l)?;
            count_tx.send(n).unwrap();
        }
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();
    send_connect_admin(&mut client);
    let _ = read_pdu(&mut client).unwrap();

    // 发 4 个 AER（cid 0xAE01..04）
    for i in 0..4 {
        send_aer(&mut client, 0xAE01 + i);
    }

    // inject 1: SMART critical 0x01/0x00/0x02 → 期望 wire 上 1 条 CapsuleResp
    inject_tx.send((0x01, 0x00, 0x02)).unwrap();
    assert_eq!(count_rx.recv().unwrap(), 1, "第 1 次 inject 应 emit 1 条");
    let resp = read_pdu(&mut client).unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let cdw0 = u32::from_le_bytes(resp.psh[..4].try_into().unwrap());
    assert_eq!(cdw0 & 0x07, 0x01, "AEN type"); // bits 2:0
    assert_eq!((cdw0 >> 8) & 0xff, 0x00, "AEN info"); // bits 15:8
    assert_eq!((cdw0 >> 16) & 0xff, 0x02, "log_id"); // bits 23:16
    let cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
    assert_eq!(cid, 0xAE01, "FIFO 第 1 条 AER cid");

    // inject 2: NOTICE 0x02/0x05/0x81 → 期望第 2 条
    inject_tx.send((0x02, 0x05, 0x81)).unwrap();
    assert_eq!(count_rx.recv().unwrap(), 1);
    let resp = read_pdu(&mut client).unwrap();
    let cdw0 = u32::from_le_bytes(resp.psh[..4].try_into().unwrap());
    assert_eq!(cdw0 & 0x07, 0x02);
    let cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
    assert_eq!(cid, 0xAE02, "FIFO 第 2 条 AER cid");

    // inject 3: 第 3 条 AER 应被消费
    inject_tx.send((0x00, 0x01, 0x01)).unwrap();
    assert_eq!(count_rx.recv().unwrap(), 1);
    let resp = read_pdu(&mut client).unwrap();
    let cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
    assert_eq!(cid, 0xAE03, "FIFO 第 3 条 AER cid");

    // inject 4: 第 4 条 AER 应被消费
    inject_tx.send((0x07, 0x00, 0xFF)).unwrap();
    assert_eq!(count_rx.recv().unwrap(), 1);
    let resp = read_pdu(&mut client).unwrap();
    let cid = u16::from_le_bytes(resp.psh[12..14].try_into().unwrap());
    assert_eq!(cid, 0xAE04, "FIFO 第 4 条 AER cid");

    // inject 5: 无 AER 可弹 → 返 0 不发 wire
    inject_tx.send((0x01, 0x00, 0x02)).unwrap();
    assert_eq!(count_rx.recv().unwrap(), 0, "无 AER 时 inject 应 drop");

    // 让 thread 退出
    drop(inject_tx);
    h.join().unwrap().unwrap();

    // wire 上确认没有第 5 条 CapsuleResp（设短 read timeout 撞 timeout）
    client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let r = read_pdu(&mut client);
    assert!(
        r.is_err(),
        "应 timeout / EOF，而不是收到第 5 条 CapsuleResp"
    );
}

/// **V6c-2** — AER 与 IO 路径不冲突：发 AER 后跟 Identify Controller，
/// 验 Identify 仍正常返 C2HData + CapsuleResp。
#[test]
fn v6c_e2e_aer_interleaved_with_admin_identify() {
    let (mut client, server) = tcp_pair();
    let (controller, _backing) = make_controller();
    let h = thread::spawn(move || -> anyhow::Result<()> {
        let mut sess = V2Session::accept_and_handshake(server, controller)?;
        sess.pump_one()?; // Connect admin
        sess.pump_one()?; // AER
        sess.pump_one()?; // Identify
        Ok(())
    });
    send_icreq(&mut client);
    let _ = read_pdu(&mut client).unwrap();
    send_connect_admin(&mut client);
    let _ = read_pdu(&mut client).unwrap();

    send_aer(&mut client, 0xAE99);

    // Identify Controller (CNS=1)
    let mut sqe = [0u8; 64];
    sqe[0] = 0x06; // IDENTIFY
    sqe[2..4].copy_from_slice(&0x00C9u16.to_le_bytes());
    sqe[40..44].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // CNS=1
    let hdr = CommonHdr {
        pdu_type: pdu_type::CMD,
        flags: 0,
        hlen: 72,
        pdo: 0,
        plen: 72,
    };
    write_pdu(&mut client, &hdr, &sqe, &[]).unwrap();

    let p = read_pdu(&mut client).unwrap();
    assert_eq!(p.header.pdu_type, pdu_type::C2H_DATA);
    let resp = read_pdu(&mut client).unwrap();
    assert_eq!(resp.header.pdu_type, pdu_type::RSP);
    let sc = ((u16::from_le_bytes(resp.psh[14..16].try_into().unwrap()) >> 1) & 0xff) as u8;
    assert_eq!(sc, 0, "Identify Controller 在 AER pending 下仍 success");
    h.join().unwrap().unwrap();
}
