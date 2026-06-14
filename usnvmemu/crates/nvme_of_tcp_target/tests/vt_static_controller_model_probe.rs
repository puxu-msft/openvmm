// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **static controller model probe** — 实测 usnvmemu NVMe-oF target 在 host 显式
//! 请求特定 CNTLID（static controller model）时的真实 wire 行为。
//!
//! # 背景：为什么这是 Linux 测试盖不到的面
//!
//! NVMe-oF Connect data（spec § 3.3 Figure 20）有 CNTLID 字段：
//! - `0xFFFF` = **dynamic controller model**（host 不指定，target 任分一个）
//! - `0xFFFE` = static controller model「any available static controller」
//! - 其它值  = static controller model 指定**具体** controller id
//!
//! Linux `nvme-cli` / `nvme-tcp.ko` 默认走 **dynamic**（发 `0xFFFF`），所以
//! 现有全部 Linux interop 测试都只覆盖 dynamic 路径。**Windows inbox NVMe-oF
//! initiator（`stornvmeofi` / `nvmeofutil`）默认 static controller model**
//! (`add SubsystemPort -dy false`，`connect -ci <controller id>`)——这条
//! 路径从未被任何 Linux 测试触及。
//!
//! 本 probe 用 raw wire（不经 nvme-cli/kernel，因为 kernel host 控制不了
//! 发什么 CNTLID）craft 一个 Connect capsule，把 `cd.cntlid` 设成具体值，
//! 观测 target 回的 CQE.result DW0（= 实际分配的 CNTLID）与 SC，坐实当前
//! 实现对 static 请求是 **honor / 静默 coerce / reject** 哪一种。
//!
//! # 实测结论（见各 test 的断言 + 注释）
//!
//! `handle_connect_async`（`src/async_session.rs`）解析 `ConnectData` 但
//! **从不读 `cd.cntlid`**，无条件返回 `TEACHING_CNTLID = 1`
//! （`src/fabric.rs`）。Discovery Log 侧亦写死 `cntlid = 0xFFFF`
//! （`nvme_firmware/src/controller/discovery_log.rs`）。即 usnvmemu 是
//! **dynamic-controller-model-only**：对 static 的具体-CNTLID 请求**静默
//! coerce 成 1**，既不 honor 也不按 spec reject。本 probe 把这个行为锁成
//! regression gate，供未来修复（让 Connect 真读 cd.cntlid 并按 spec 校验）
//! 时翻红。

#![allow(missing_docs)]

use nvme_firmware::NvmeController;
use nvme_of_tcp_target::fabric::{self, ConnectData, ConnectFabricFields, fctype};
use nvme_of_tcp_target::framing::{Pdu, read_pdu_async, write_pdu_async};
use nvme_of_tcp_target::pdu::{CommonHdr, IcPsh, pdu_type};
use nvme_of_tcp_target::{AsyncSession, SharedControllerInner, accept_and_handshake_async};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use zerocopy::IntoBytes;

fn make_shared() -> (Arc<SharedControllerInner>, tempfile::NamedTempFile) {
    let f = tempfile::NamedTempFile::new().unwrap();
    f.as_file().set_len(16 * 1024 * 1024).unwrap();
    let path = f.path().to_string_lossy().into_owned();
    let c = NvmeController::open(std::slice::from_ref(&path), 0x1414, 0, &[]).unwrap();
    (Arc::new(SharedControllerInner::new(c)), f)
}

fn build_icreq() -> (CommonHdr, Vec<u8>) {
    let hdr = CommonHdr {
        pdu_type: pdu_type::ICREQ,
        flags: 0,
        hlen: 128,
        pdo: 0,
        plen: 128,
    };
    (hdr, IcPsh::default().as_bytes().to_vec())
}

/// 与 v_interop_2 的 `build_connect` 相同，但**显式设 `cd.cntlid`**——这正是
/// 本 probe 的核心变量（Linux nvme-cli 永远发 0xFFFF，构造不出别的）。
fn build_connect_with_cntlid(
    cid: u16,
    qid: u16,
    kato_ms: u32,
    hostnqn: &str,
    subnqn: &str,
    cntlid: u16,
) -> Pdu {
    let mut sqe = vec![0u8; 64];
    sqe[0] = fabric::NVME_OPC_FABRIC;
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe[4] = fctype::CONNECT;
    let f = ConnectFabricFields {
        recfmt: 0,
        qid,
        sqsize: 31, // 0-based → qsize=32
        cattr: 0,
        rsvd1: 0,
        kato: kato_ms,
        rsvd2: [0u8; 12],
    };
    sqe[40..64].copy_from_slice(f.as_bytes());
    let mut cd = ConnectData {
        cntlid, // ← static controller model：host 请求的具体 CNTLID
        ..ConnectData::default()
    };
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

fn cqe_sc(pdu: &Pdu) -> u8 {
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP);
    let status = u16::from_le_bytes([pdu.psh[14], pdu.psh[15]]);
    ((status >> 1) & 0xFF) as u8
}

/// CapsuleResp PSH byte 0..4 = CQE result DW0 = target 实际分配的 CNTLID（低 16 位）。
fn cqe_assigned_cntlid(pdu: &Pdu) -> u16 {
    assert_eq!(pdu.header.pdu_type, pdu_type::RSP);
    u16::from_le_bytes([pdu.psh[0], pdu.psh[1]])
}

async fn send_icreq(c: &mut TcpStream) {
    let (h, psh) = build_icreq();
    write_pdu_async(c, &h, &psh, &[]).await.unwrap();
    let r = read_pdu_async(c).await.unwrap();
    assert_eq!(r.header.pdu_type, pdu_type::ICRESP);
}

/// 启单 conn listener，对每条 conn 跑 handshake + pump/dispatch 循环。
async fn spawn_listener(
    shared: Arc<SharedControllerInner>,
) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                r = listener.accept() => {
                    if let Ok((server, _)) = r {
                        let s = Arc::clone(&shared);
                        tokio::spawn(async move {
                            let mut sess: AsyncSession<TcpStream> =
                                accept_and_handshake_async(server, s).await.unwrap();
                            let (_tx, mut rx) = tokio::sync::watch::channel(false);
                            for _ in 0..8 {
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
                    }
                }
            }
        }
    });
    (addr, shutdown_tx)
}

/// 单条 admin Connect，返回 (SC, 分配的 CNTLID)。
async fn connect_once(addr: std::net::SocketAddr, requested_cntlid: u16) -> (u8, u16) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    send_icreq(&mut s).await;
    let connect = build_connect_with_cntlid(
        0x0042, // cid != 1：刻意非 1，让 baseline 的 assigned==1 断言与 cid 数值去耦
        0,
        10000,
        "nqn.2014-08.org.nvmexpress:uuid:host-01",
        "nqn.2014-08.org.nvmexpress:teaching:disk",
        requested_cntlid,
    );
    write_pdu_async(&mut s, &connect.header, &connect.psh, &connect.data)
        .await
        .unwrap();
    let r = read_pdu_async(&mut s).await.unwrap();
    (cqe_sc(&r), cqe_assigned_cntlid(&r))
}

/// **baseline（dynamic）** — host 发 0xFFFF（Linux nvme-cli 真实行为）。
/// target 分配 TEACHING_CNTLID=1，SC=0。这是现有 Linux interop 已覆盖的面。
#[tokio::test(flavor = "multi_thread")]
async fn dynamic_request_0xffff_assigns_cntlid_1() {
    let (shared, _backing) = make_shared();
    let (addr, shutdown_tx) = spawn_listener(Arc::clone(&shared)).await;
    let (sc, assigned) = connect_once(addr, 0xFFFF).await;
    assert_eq!(sc, 0, "dynamic Connect (0xFFFF) 应 SC=0");
    assert_eq!(assigned, 1, "dynamic 模型分配 TEACHING_CNTLID=1");
    let _ = shutdown_tx.send(true);
}

/// **decisive probe（static specific）** — host 请求具体 CNTLID=5
/// （static controller model，Windows `connect -ci 5` 类行为）。
///
/// **实测锁定的当前行为 = 静默 coerce**：target 无视请求值，仍返 SC=0 +
/// 分配 CNTLID=1。即 host 要 5、拿到 1，且**没有错误指示**。
///
/// spec § 3.3：static controller model 下，若请求的具体 CNTLID 不对应一个
/// 可用 controller，应返 fabric status「Connect Invalid Parameters」(SC=0x82)
/// 而非静默换一个。本断言把「静默 coerce」锁成 regression gate——未来修复
/// （Connect 真读 cd.cntlid 并校验）时本测试应翻红并被更新。
#[tokio::test(flavor = "multi_thread")]
async fn static_specific_request_is_silently_coerced_to_1() {
    let (shared, _backing) = make_shared();
    let (addr, shutdown_tx) = spawn_listener(Arc::clone(&shared)).await;
    let (sc, assigned) = connect_once(addr, 5).await;
    // 当前实现：handle_connect_async 不读 cd.cntlid → 静默返 1。
    assert_eq!(
        sc, 0,
        "当前实现对 static 具体-CNTLID 请求**不 reject**（既不 honor 也不按 spec 拒）"
    );
    assert_eq!(
        assigned, 1,
        "当前实现把 host 请求的 CNTLID=5 静默 coerce 成 TEACHING_CNTLID=1"
    );
    let _ = shutdown_tx.send(true);
}

/// **static-any** — host 发 0xFFFE（static 模型「任一 static controller」）。
/// 当前实现同样静默返 1。0xFFFE 语义下返 1 尚算可接受（任分一个 static），
/// 但与具体值请求走的是同一条「不读 cd.cntlid」代码路径，一并锁定。
#[tokio::test(flavor = "multi_thread")]
async fn static_any_request_0xfffe_assigns_cntlid_1() {
    let (shared, _backing) = make_shared();
    let (addr, shutdown_tx) = spawn_listener(Arc::clone(&shared)).await;
    let (sc, assigned) = connect_once(addr, 0xFFFE).await;
    assert_eq!(sc, 0, "static-any (0xFFFE) Connect SC=0");
    assert_eq!(assigned, 1, "static-any 分配 CNTLID=1");
    let _ = shutdown_tx.send(true);
}
