// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V9 RDMA R3b** — `RdmaFabricBackend`：NVMe-oF RDMA 的 [`FabricBackend`] 实现，over
//! [`rdma_transport::RdmaVerbs`]（R3b 用 `MockRdma`，R5a 换真 `IbverbsRdma`）。
//!
//! 把上层桥（`run_post_dispatch`）的 transport-无关数据移动落到 RDMA verbs：
//! - **recv_next** = `poll_cq` 取 RECV completion → 读 recv MR → **合成 CapsuleCmd PDU**
//!   （复用 `dispatch_pdu_async` 全路径，同「假-GPA 桥」合成思路）→ repost recv buffer。
//! - **send_response_capsule** = `post_send`（16B CQE）。
//! - **move_local_to_host**（读命令 data-out）= `post_rdma_write` 到 host keyed-SGL。
//! - **move_host_to_local**（写命令 data-in）= `post_rdma_read` 从 host keyed-SGL 填入 dst。
//!
//! **R3b 边界（单段 keyed-SGL fast-reg 路径）**：multi-segment SGL list（msdbd>1）/ separate-meta
//! PI / in-capsule inline / SEND_WITH_INV / 预注册 MR 池都是 **R4**；本 phase 遇到非单段 keyed
//! 即 bail，把边界显式化（R0 §E）。**MR 每调用 reg（慢）也留 R4 预注册池**（R0 §4 遗漏 E）。
//!
//! **CQ demux**：`poll_cq` 返回 RECV + send-side 混合 completion；send-side（WRITE/READ/SEND 完成）
//! 由发起它的数据移动方法 `await_send` 消费，RECV 缓冲给 recv_next（per-QP 单 CQ，R0 §F）。

use std::collections::HashMap;
use std::collections::VecDeque;

use anyhow::Context as _;
use rdma_transport::{Access, Opcode, RdmaVerbs, Rkey, WcStatus, WrId};

use crate::fabric_backend::{FabricBackend, HostBuf, KeyedSeg, RecvFrame};
use crate::pdu::{CommonHdr, pdu_type};

/// 一个 NVMe SQE 字节数（命令胶囊的 PSH）。
const SQE_BYTES: usize = 64;
/// CapsuleCmd PDU 的 hlen = CH(8) + SQE(64)。
const CAPSULE_CMD_HLEN: u8 = 8 + SQE_BYTES as u8;

/// NVMe-oF **RDMA** 的 [`FabricBackend`]，over `V: RdmaVerbs`（per-QP，R0 §F）。
pub struct RdmaFabricBackend<V: RdmaVerbs> {
    verbs: V,
    /// 已 post 的 recv buffer：wr_id → MR（RECV 完成时读它取胶囊）。
    recv_pool: HashMap<WrId, rdma_transport::MrHandle>,
    /// recv buffer 字节数（≥ 单胶囊：64B SQE + 可选 inline，R3b 取 SQE+小余量）。
    recv_buf_size: usize,
    /// recv post 的下一个 wr_id。
    next_recv_wr: WrId,
    /// send-side（SEND/READ/WRITE）的下一个 wr_id。
    next_send_wr: WrId,
    /// drain 时缓冲的 RECV completion（wr_id, byte_len）。
    pending_recvs: VecDeque<(WrId, u32)>,
    /// drain 时缓冲的 send-side completion（wr_id → 状态）。
    pending_sends: HashMap<WrId, WcStatus>,
}

impl<V: RdmaVerbs> RdmaFabricBackend<V> {
    /// 新建并 post `recv_depth` 个 recv buffer（RNR 避免：始终有 buffer 接对端 SEND）。
    /// `recv_buf_size` ≥ 单命令胶囊（64B SQE [+ inline，R4]）。
    pub fn new(verbs: V, recv_depth: usize, recv_buf_size: usize) -> anyhow::Result<Self> {
        let mut me = Self {
            verbs,
            recv_pool: HashMap::new(),
            recv_buf_size: recv_buf_size.max(SQE_BYTES),
            next_recv_wr: 1,
            // send-side wr_id 与 recv wr_id 分段（高位起），避免 demux 混淆。
            next_send_wr: 1 << 32,
            pending_recvs: VecDeque::new(),
            pending_sends: HashMap::new(),
        };
        for _ in 0..recv_depth {
            me.post_one_recv()?;
        }
        Ok(me)
    }

    /// 暴露内部 verbs（测试断言 / teardown）。
    pub fn verbs_mut(&mut self) -> &mut V {
        &mut self.verbs
    }

    /// post 一个新 recv buffer 进池。
    fn post_one_recv(&mut self) -> anyhow::Result<()> {
        let mr = self
            .verbs
            .reg_mr(vec![0u8; self.recv_buf_size], Access::local_write())
            .context("R3b reg recv MR")?;
        let wr = self.next_recv_wr;
        self.next_recv_wr += 1;
        self.verbs.post_recv(mr, wr).context("R3b post_recv")?;
        self.recv_pool.insert(wr, mr);
        Ok(())
    }

    fn next_send(&mut self) -> WrId {
        let wr = self.next_send_wr;
        self.next_send_wr += 1;
        wr
    }

    /// poll CQ，把 completion 按类分流：RECV → `pending_recvs`，send-side → `pending_sends`。
    fn drain_cq(&mut self) {
        for c in self.verbs.poll_cq() {
            match c.opcode {
                Opcode::Recv => self.pending_recvs.push_back((c.wr_id, c.byte_len)),
                _ => {
                    self.pending_sends.insert(c.wr_id, c.status);
                }
            }
        }
    }

    /// 等某个 send-side WR 完成（mock 即时物化；真 verbs 此处 await CQ event，R5a）。
    /// `MAX_DRAIN` 防 mock 误用时死循环（真 verbs 会阻塞等通知，不靠此）。
    /// **前提**：依赖 mock 的 `poll_cq` 单次即物化全部 in-flight send work；若 mock 改为分多次
    /// 物化（如建模 ORD 背压部分完成），此 busy-drain 形态须随真 verbs 一并重写（R5a）。
    async fn await_send(&mut self, wr: WrId) -> anyhow::Result<()> {
        const MAX_DRAIN: usize = 1024;
        for _ in 0..MAX_DRAIN {
            if let Some(status) = self.pending_sends.remove(&wr) {
                return match status {
                    WcStatus::Success => Ok(()),
                    WcStatus::Error(e) => {
                        anyhow::bail!("R3b send-side WR {wr} failed: {e:?}")
                    }
                };
            }
            self.drain_cq();
        }
        anyhow::bail!(
            "R3b await_send: WR {wr} never completed (mock 未物化?；真 verbs 应 await CQ event)"
        )
    }

    /// 取 host 的**单段** keyed-SGL（R3b 边界）。多段/separate-meta/inline/FlowControlled 一律 bail。
    fn single_keyed_seg(host: &HostBuf) -> anyhow::Result<KeyedSeg> {
        match host {
            HostBuf::Keyed { data, meta: None } if data.len() == 1 => Ok(data[0]),
            HostBuf::Keyed { data, meta } => anyhow::bail!(
                "R3b 仅支持单段 keyed-SGL；得到 {} 段 + meta={} → multi-SGL/separate-meta 是 R4",
                data.len(),
                meta.is_some()
            ),
            HostBuf::Inline { .. } => anyhow::bail!("R3b 不支持 in-capsule inline（R4）"),
            HostBuf::FlowControlled { .. } => {
                anyhow::bail!("RdmaFabricBackend 收到 FlowControlled（TCP-only HostBuf），非法")
            }
        }
    }

    /// 把收到的胶囊字节合成成 CapsuleCmd PDU（psh = 64B SQE；data = inline，R3b 应空）。
    fn synthesize_capsule_pdu(capsule: &[u8]) -> anyhow::Result<crate::framing::Pdu> {
        if capsule.len() < SQE_BYTES {
            anyhow::bail!("R3b recv 胶囊 {}B < 64B SQE", capsule.len());
        }
        let sqe = capsule[..SQE_BYTES].to_vec();
        let inline = capsule[SQE_BYTES..].to_vec(); // in-capsule data（R4 inline；R3b 期望空）
        Ok(crate::framing::Pdu {
            header: CommonHdr {
                pdu_type: pdu_type::CMD,
                flags: 0,
                hlen: CAPSULE_CMD_HLEN,
                pdo: CAPSULE_CMD_HLEN,
                plen: CAPSULE_CMD_HLEN as u32 + inline.len() as u32,
            },
            psh: sqe,
            data: inline,
        })
    }
}

impl<V: RdmaVerbs> FabricBackend for RdmaFabricBackend<V> {
    async fn send_response_capsule(&mut self, cqe: &[u8; 16]) -> anyhow::Result<()> {
        // SEND_WITH_INV（远端失效 rkey）是 R4；R3b 用普通 SEND。MR 每调用 reg（预注册池 R4）。
        let mr = self
            .verbs
            .reg_mr(cqe.to_vec(), Access::local_read())
            .context("R3b reg response-capsule MR")?;
        let wr = self.next_send();
        self.verbs
            .post_send(mr, 16, wr, None)
            .context("R3b post_send response capsule")?;
        self.await_send(wr).await
    }

    async fn move_local_to_host(
        &mut self,
        _cid: u16,
        host: &HostBuf,
        data: &[u8],
        data_offset: u32,
        _is_last: bool,
    ) -> anyhow::Result<()> {
        // 读命令 data-out：RDMA WRITE 推 data 到 host keyed-SGL。`is_last` 在 RDMA 无对应
        // （完成由 send_response 的 SEND 表达，architect R2 note）。
        let seg = Self::single_keyed_seg(host)?;
        let remote = seg
            .remote_addr
            .checked_add(data_offset as u64)
            .context("R3b RDMA WRITE remote_addr+offset 溢出")?;
        let mr = self
            .verbs
            .reg_mr(data.to_vec(), Access::local_read())
            .context("R3b reg WRITE-src MR")?;
        let wr = self.next_send();
        self.verbs
            .post_rdma_write(mr, 0, remote, Rkey(seg.rkey), data.len() as u32, wr)
            .context("R3b post_rdma_write")?;
        self.await_send(wr).await
    }

    async fn move_host_to_local(
        &mut self,
        _cid: u16,
        host: &HostBuf,
        base_offset: u32,
        dst: &mut [u8],
    ) -> anyhow::Result<()> {
        // 写命令 data-in：RDMA READ 从 host keyed-SGL 拉到本地 MR，再填 dst。
        let seg = Self::single_keyed_seg(host)?;
        let remote = seg
            .remote_addr
            .checked_add(base_offset as u64)
            .context("R3b RDMA READ remote_addr+offset 溢出")?;
        let mr = self
            .verbs
            .reg_mr(vec![0u8; dst.len()], Access::local_write())
            .context("R3b reg READ-dst MR")?;
        let wr = self.next_send();
        self.verbs
            .post_rdma_read(mr, 0, remote, Rkey(seg.rkey), dst.len() as u32, wr)
            .context("R3b post_rdma_read")?;
        self.await_send(wr).await?;
        let bytes = self.verbs.mr_bytes(mr).context("R3b READ-dst MR 消失")?;
        dst.copy_from_slice(&bytes[..dst.len()]);
        Ok(())
    }

    async fn recv_next(&mut self) -> anyhow::Result<RecvFrame> {
        // 先用缓冲的 RECV，否则 drain 一次 CQ。mock：对端 inject_send 后本次 drain 即得；
        // 真 verbs（R5a）此处 await CQ event channel 真阻塞 + 取消安全（poll_cq 本身 cancel-safe）。
        if self.pending_recvs.is_empty() {
            self.drain_cq();
        }
        let Some((wr, byte_len)) = self.pending_recvs.pop_front() else {
            // R3b 单元语境：调用方应已 inject。真集成（R3c/d）由 pump select! 驱动，
            // recv 无数据时 await 而非返错——届时 recv_next 接 CQ event。
            anyhow::bail!(
                "R3b recv_next: 无入站 completion（mock 需先 inject；真 verbs await CQ event = R5a）"
            );
        };
        let mr = self
            .recv_pool
            .remove(&wr)
            .with_context(|| format!("R3b RECV wr_id {wr} 无对应 recv MR"))?;
        // 只取 completion 报告的实收字节数（recv MR 可能比胶囊大）。
        let full = self.verbs.mr_bytes(mr).context("R3b recv MR 消失")?;
        let n = (byte_len as usize).min(full.len());
        let capsule = full[..n].to_vec();
        let pdu = Self::synthesize_capsule_pdu(&capsule)?;
        // repost 一个新 recv buffer（RNR 避免：消耗一个就补一个，池深恒定）。
        self.post_one_recv()?;
        Ok(RecvFrame::Frame(pdu))
    }

    async fn terminate(&mut self, fes: u16) -> anyhow::Result<()> {
        // RDMA 无 C2HTermReq wire 对应；fatal 错误映射到 QP teardown（真 association
        // teardown / in-flight drain 是 R3c 的 QP-drain 工作，此处先记日志）。
        tracing::warn!(fes, "R3b RDMA terminate（无 C2HTerm；QP teardown 留 R3c）");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdma_transport::MockRdma;

    const RKEY: u32 = 0xABCD;
    const REMOTE_BASE: u64 = 0x1_0000_0000; // 高地址（>4 GiB，项目 §26 子条款）

    fn backend() -> RdmaFabricBackend<MockRdma> {
        RdmaFabricBackend::new(
            MockRdma::new(/*ord*/ 8),
            /*recv_depth*/ 4,
            /*buf*/ 128,
        )
        .unwrap()
    }

    /// recv_next：对端 SEND 一个 64B 命令胶囊 → 合成 CapsuleCmd PDU（psh=SQE）。
    #[tokio::test]
    async fn recv_next_synthesizes_capsule_pdu() {
        let mut be = backend();
        let mut sqe = [0u8; 64];
        sqe[0] = 0x06; // Identify opcode（任意，证 psh=SQE 透传）
        sqe[2..4].copy_from_slice(&0x1234u16.to_le_bytes()); // cid
        be.verbs_mut().inject_send(&sqe, None).unwrap();
        match be.recv_next().await.unwrap() {
            RecvFrame::Frame(pdu) => {
                assert_eq!(pdu.header.pdu_type, pdu_type::CMD);
                assert_eq!(pdu.psh.len(), 64);
                assert_eq!(pdu.psh[0], 0x06);
                assert_eq!(&pdu.psh[2..4], &0x1234u16.to_le_bytes());
                assert!(pdu.data.is_empty()); // R3b 无 inline
            }
            RecvFrame::PeerClosed => panic!("expected Frame"),
        }
    }

    /// recv pool repost：recv 一个后池深恢复（再 inject+recv 仍成功，不撞 RNR）。
    #[tokio::test]
    async fn recv_pool_reposts_after_consume() {
        let mut be = backend();
        for i in 0..6u8 {
            // 6 > 初始 recv_depth(4)：若不 repost，第 5 个 inject 就撞 RNR。
            let mut sqe = [0u8; 64];
            sqe[3] = i;
            be.verbs_mut().inject_send(&sqe, None).unwrap();
            match be.recv_next().await.unwrap() {
                RecvFrame::Frame(pdu) => assert_eq!(pdu.psh[3], i),
                RecvFrame::PeerClosed => panic!(),
            }
        }
    }

    /// move_local_to_host：RDMA WRITE 推 data 到 host keyed-SGL（验 remote 内存更新）。
    #[tokio::test]
    async fn move_local_to_host_rdma_write() {
        let mut be = backend();
        be.verbs_mut()
            .stub_remote(Rkey(RKEY), REMOTE_BASE, vec![0u8; 32]);
        let host = HostBuf::Keyed {
            data: vec![KeyedSeg {
                remote_addr: REMOTE_BASE,
                rkey: RKEY,
                len: 32,
            }],
            meta: None,
        };
        let data: Vec<u8> = (0..32).collect();
        be.move_local_to_host(0x1, &host, &data, /*offset*/ 0, /*is_last*/ true)
            .await
            .unwrap();
        assert_eq!(
            be.verbs_mut().remote_bytes(Rkey(RKEY)).unwrap(),
            (0u8..32).collect::<Vec<_>>().as_slice()
        );
    }

    /// move_host_to_local：RDMA READ 从 host keyed-SGL 拉到 dst。
    #[tokio::test]
    async fn move_host_to_local_rdma_read() {
        let mut be = backend();
        be.verbs_mut()
            .stub_remote(Rkey(RKEY), REMOTE_BASE, (100..132).collect());
        let host = HostBuf::Keyed {
            data: vec![KeyedSeg {
                remote_addr: REMOTE_BASE,
                rkey: RKEY,
                len: 32,
            }],
            meta: None,
        };
        let mut dst = vec![0u8; 32];
        be.move_host_to_local(0x1, &host, /*base_offset*/ 0, &mut dst)
            .await
            .unwrap();
        assert_eq!(dst, (100u8..132).collect::<Vec<_>>());
    }

    /// send_response_capsule：post_send 16B CQE 完成。
    #[tokio::test]
    async fn send_response_capsule_ok() {
        let mut be = backend();
        let mut cqe = [0u8; 16];
        cqe[12..14].copy_from_slice(&0x1234u16.to_le_bytes());
        be.send_response_capsule(&cqe).await.unwrap();
    }

    /// R3b 边界：multi-segment / inline / FlowControlled 一律 bail（R4 显式化）。
    #[tokio::test]
    async fn r3b_rejects_non_single_keyed() {
        let mut be = backend();
        let multi = HostBuf::Keyed {
            data: vec![
                KeyedSeg {
                    remote_addr: REMOTE_BASE,
                    rkey: RKEY,
                    len: 8,
                },
                KeyedSeg {
                    remote_addr: REMOTE_BASE + 8,
                    rkey: RKEY,
                    len: 8,
                },
            ],
            meta: None,
        };
        assert!(
            be.move_local_to_host(1, &multi, &[0u8; 16], 0, true)
                .await
                .is_err()
        );
        let inline = HostBuf::Inline {
            capsule_offset: 0,
            len: 8,
        };
        assert!(
            be.move_local_to_host(1, &inline, &[0u8; 8], 0, true)
                .await
                .is_err()
        );
        let tcp = HostBuf::FlowControlled { total_len: 8 };
        assert!(
            be.move_local_to_host(1, &tcp, &[0u8; 8], 0, true)
                .await
                .is_err()
        );
        // separate-meta（meta=Some）= R4。
        let with_meta = HostBuf::Keyed {
            data: vec![KeyedSeg {
                remote_addr: REMOTE_BASE,
                rkey: RKEY,
                len: 8,
            }],
            meta: Some(KeyedSeg {
                remote_addr: REMOTE_BASE + 0x1000,
                rkey: RKEY,
                len: 8,
            }),
        };
        assert!(
            be.move_local_to_host(1, &with_meta, &[0u8; 8], 0, true)
                .await
                .is_err()
        );
        // 空 data 段（非法输入）。
        let empty = HostBuf::Keyed {
            data: vec![],
            meta: None,
        };
        assert!(
            be.move_local_to_host(1, &empty, &[0u8; 8], 0, true)
                .await
                .is_err()
        );
    }
}
