// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `MockRdma` —— 进程内 `RdmaVerbs` 假实现，**忠实模拟** wire reference §D 语义契约
//! （而非乐观 FIFO-永不失败）。让 RDMA target 逻辑（R2 FabricBackend / R3 CM+admin /
//! R4 数据路径）在无内核 RDMA 设备下全测；R5a 真 `IbverbsRdma` 是 drop-in 替换。
//!
//! 模拟范围（target 侧单 QP 视角）：
//! - 本地 MR 注册表（`reg_mr`）。
//! - **模拟的远端（host）内存**：按 rkey 索引（测试用 [`MockRdma::stub_remote`] 布置，
//!   模拟 host 经 keyed-SGL 暴露的 MR）；RDMA READ/WRITE 对它读写。
//! - **work 在 `poll_cq` 时按提交顺序物化**（§D.1 保序）：单 `work` FIFO 队列。
//! - **QP error → flush → 重建**（§D.2）：一个 WR 出错 → 该 completion Error + QP 进 Error +
//!   队列里**后续 WR 全 flush 成 `WrFlushError`** + 新 post 被拒，直到 [`MockRdma::recreate_qp`]。
//! - **RNR**（§D.3）：对端 SEND（[`MockRdma::inject_send`]）撞无 posted recv buffer → RNR。
//! - **ORD 限深**（§D.4）：in-flight RDMA READ 超 `ord_max` → `OrdExceeded`。
//!
//! 测试钩子（非 trait，仅 `MockRdma`）：`stub_remote` / `remote_bytes` / `inject_send` /
//! `inject_wc_error` / `recreate_qp`，模拟「对端 host」行为。

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::verbs::{
    Access, Completion, MrHandle, Opcode, QpState, RdmaError, RdmaVerbs, Rkey, WcError, WcStatus,
    WrId,
};

/// 一块本地已注册 MR。
struct Mr {
    bytes: Vec<u8>,
    #[allow(dead_code)] // access 在 mock 不强制校验权限，留作真实 impl 对齐 + 文档化意图
    access: Access,
}

/// 模拟的远端（host）内存区，对应 host 经 keyed-SGL 暴露的一个 MR。
#[derive(Clone)]
struct RemoteRegion {
    base_addr: u64,
    bytes: Vec<u8>,
}

/// 一条待 poll 物化的 work。recv 由 `inject_send` 直接产 completion（对端驱动），故不在此。
struct PendingWr {
    wr_id: WrId,
    op: PendingOp,
    /// 测试经 `inject_wc_error` 强制本 WR 完成为该错误。
    forced_error: Option<WcError>,
}

enum PendingOp {
    Send,
    RdmaRead {
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
    },
    RdmaWrite {
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
    },
}

/// 进程内 `RdmaVerbs` 假实现（一个 QP 的操作面）。
pub struct MockRdma {
    mrs: HashMap<u64, Mr>,
    next_mr: u64,
    remote: HashMap<u32, RemoteRegion>,
    /// 已 post 待物化的 send-side work（FIFO，§D.1 保序）。
    work: VecDeque<PendingWr>,
    /// 已 post 的 recv buffer（FIFO）。
    recv_bufs: VecDeque<(MrHandle, WrId)>,
    /// 对端 SEND 已投递、等被 poll 取走的 recv completion（按到达序）。
    recv_done: VecDeque<Completion>,
    qp: QpState,
    /// outgoing RDMA READ 并发上限（§D.4 IRD/ORD）。
    ord_max: u32,
    /// 当前 in-flight（已 post 未 poll 完成）的 RDMA READ 数。
    outstanding_reads: u32,
    /// 测试经 `inject_wc_error` 设；作用于**下一个** post 的 send-side WR。
    inject_next_error: Option<WcError>,
}

impl MockRdma {
    /// 新建一个就绪 QP。`ord_max` = outgoing RDMA READ 并发上限（典型真值由 CM 协商，
    /// 测试可设小值验限深）。
    pub fn new(ord_max: u32) -> Self {
        Self {
            mrs: HashMap::new(),
            next_mr: 1,
            remote: HashMap::new(),
            work: VecDeque::new(),
            recv_bufs: VecDeque::new(),
            recv_done: VecDeque::new(),
            qp: QpState::Ready,
            ord_max,
            outstanding_reads: 0,
            inject_next_error: None,
        }
    }

    // ---- 测试钩子（模拟对端 host）----

    /// 布置一块模拟 host MR：`rkey` 处、远端地址从 `base_addr` 起、内容 `bytes`。
    /// target 的 RDMA READ/WRITE 对它读写。
    pub fn stub_remote(&mut self, rkey: Rkey, base_addr: u64, bytes: Vec<u8>) {
        self.remote
            .insert(rkey.0, RemoteRegion { base_addr, bytes });
    }

    /// 读回模拟 host MR 内容（断言 RDMA WRITE 是否落地）。
    pub fn remote_bytes(&self, rkey: Rkey) -> Option<&[u8]> {
        self.remote.get(&rkey.0).map(|r| r.bytes.as_slice())
    }

    /// 模拟对端 host SEND 一个命令胶囊到本 target。消耗最旧 posted recv buffer，把
    /// `capsule` 拷进去并产一条 `Recv` completion；`invalidate` 模拟 host 经 keyed-SGL
    /// subtype INVALIDATE 请求失效（completion 带回 rkey）。
    ///
    /// 无 posted recv buffer → 返 `Err(RnrRetryExceeded)`（§D.3 RNR）。
    pub fn inject_send(&mut self, capsule: &[u8], invalidate: Option<Rkey>) -> Result<(), WcError> {
        let (mr_handle, wr_id) = self
            .recv_bufs
            .pop_front()
            .ok_or(WcError::RnrRetryExceeded)?;
        let mr = self
            .mrs
            .get_mut(&mr_handle.0)
            // 不可达：`mrs` append-only（reg_mr 只插不删），post_recv 入队前已校验句柄存在。
            // 若将来加 dereg_mr hook，须改为 graceful 处理而非 expect。
            .expect("posted recv MR must exist (mrs is append-only)");
        let n = capsule.len().min(mr.bytes.len());
        mr.bytes[..n].copy_from_slice(&capsule[..n]);
        self.recv_done.push_back(Completion {
            wr_id,
            opcode: Opcode::Recv,
            status: WcStatus::Success,
            byte_len: n as u32,
            invalidated_rkey: invalidate,
        });
        Ok(())
    }

    /// 强制**下一个** post 的 send-side WR（SEND / RDMA READ / RDMA WRITE）完成为该错误，
    /// 触发 QP error + flush（§D.2）。
    pub fn inject_wc_error(&mut self, err: WcError) {
        self.inject_next_error = Some(err);
    }

    /// 重建 QP（模拟 QP teardown + 重建，§D.2）：清 error 态 + 丢弃 in-flight send-side work +
    /// 清 posted recv WR。真 QP→ERR 时 posted-but-unconsumed recv WR 会以 `WrFlushError` 完成并
    /// 消失（[`poll_cq`] 已在 QP 进 error 那次 poll 把它们 flush 进 CQ），重建后**须重新
    /// `post_recv`**——若 mock 留着 `recv_bufs` 会教 R3/R4 错误的 recv-reposting 模型
    /// （review-not-optional-self-consistent-trap）。此处再清一次作防御（正常已空）。
    /// `recv_done`（已物化的 completion，CQ 条目）保留可取。
    pub fn recreate_qp(&mut self) {
        self.qp = QpState::Ready;
        self.work.clear();
        self.recv_bufs.clear();
        self.outstanding_reads = 0;
        self.inject_next_error = None;
    }

    // ---- 内部 ----

    fn mr_len(&self, mr: MrHandle) -> Option<u32> {
        self.mrs.get(&mr.0).map(|m| m.bytes.len() as u32)
    }

    fn check_local(&self, mr: MrHandle, off: u32, len: u32) -> Result<(), RdmaError> {
        let mr_len = self.mr_len(mr).ok_or(RdmaError::BadMr(mr))?;
        if off.checked_add(len).is_none_or(|end| end > mr_len) {
            return Err(RdmaError::OutOfBounds { off, len, mr_len });
        }
        Ok(())
    }

    /// 物化一条 send-side WR，返回其 completion。可能改 QP 状态 / 远端内存 / 本地 MR。
    fn execute(&mut self, wr: PendingWr) -> Completion {
        let opcode = match wr.op {
            PendingOp::Send => Opcode::Send,
            PendingOp::RdmaRead { .. } => Opcode::RdmaRead,
            PendingOp::RdmaWrite { .. } => Opcode::RdmaWrite,
        };
        // 注入错误优先。
        if let Some(err) = wr.forced_error {
            if matches!(wr.op, PendingOp::RdmaRead { .. }) {
                self.outstanding_reads = self.outstanding_reads.saturating_sub(1);
            }
            return Completion {
                wr_id: wr.wr_id,
                opcode,
                status: WcStatus::Error(err),
                byte_len: 0,
                invalidated_rkey: None,
            };
        }
        let status = match wr.op {
            PendingOp::Send => WcStatus::Success,
            PendingOp::RdmaRead {
                local,
                local_off,
                remote_addr,
                rkey,
                len,
            } => {
                self.outstanding_reads = self.outstanding_reads.saturating_sub(1);
                self.do_remote_read(local, local_off, remote_addr, rkey, len)
            }
            PendingOp::RdmaWrite {
                local,
                local_off,
                remote_addr,
                rkey,
                len,
            } => self.do_remote_write(local, local_off, remote_addr, rkey, len),
        };
        Completion {
            wr_id: wr.wr_id,
            opcode,
            status,
            byte_len: 0,
            invalidated_rkey: None,
        }
    }

    /// 从远端读到本地 MR。rkey 无效 / 远端越界 → `RemoteAccessError`。
    fn do_remote_read(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
    ) -> WcStatus {
        let region = match self.remote.get(&rkey.0) {
            Some(r) => r,
            None => return WcStatus::Error(WcError::RemoteAccessError),
        };
        let Some(start) = remote_addr.checked_sub(region.base_addr) else {
            return WcStatus::Error(WcError::RemoteAccessError);
        };
        let (start, len_us) = (start as usize, len as usize);
        let Some(end) = start.checked_add(len_us) else {
            return WcStatus::Error(WcError::RemoteAccessError);
        };
        if end > region.bytes.len() {
            return WcStatus::Error(WcError::RemoteAccessError);
        }
        let data = region.bytes[start..end].to_vec();
        match self.mrs.get_mut(&local.0) {
            Some(mr) => {
                let (lo, le) = (local_off as usize, local_off as usize + len_us);
                if le > mr.bytes.len() {
                    return WcStatus::Error(WcError::LocalProtectionError);
                }
                mr.bytes[lo..le].copy_from_slice(&data);
                WcStatus::Success
            }
            None => WcStatus::Error(WcError::LocalProtectionError),
        }
    }

    /// 从本地 MR 写到远端。
    fn do_remote_write(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
    ) -> WcStatus {
        let (lo, le) = (local_off as usize, local_off as usize + len as usize);
        let data = match self.mrs.get(&local.0) {
            Some(mr) if le <= mr.bytes.len() => mr.bytes[lo..le].to_vec(),
            Some(_) => return WcStatus::Error(WcError::LocalProtectionError),
            None => return WcStatus::Error(WcError::LocalProtectionError),
        };
        let region = match self.remote.get_mut(&rkey.0) {
            Some(r) => r,
            None => return WcStatus::Error(WcError::RemoteAccessError),
        };
        let Some(start) = remote_addr.checked_sub(region.base_addr) else {
            return WcStatus::Error(WcError::RemoteAccessError);
        };
        let (start, end) = (start as usize, start as usize + len as usize);
        if end > region.bytes.len() {
            return WcStatus::Error(WcError::RemoteAccessError);
        }
        region.bytes[start..end].copy_from_slice(&data);
        WcStatus::Success
    }

    /// 取出下一个注入错误（每次 post 消费一次）。
    fn take_forced_error(&mut self) -> Option<WcError> {
        self.inject_next_error.take()
    }

    fn ensure_ready(&self) -> Result<(), RdmaError> {
        if self.qp == QpState::Error {
            return Err(RdmaError::QpInError);
        }
        Ok(())
    }
}

impl RdmaVerbs for MockRdma {
    fn reg_mr(&mut self, bytes: Vec<u8>, access: Access) -> Result<MrHandle, RdmaError> {
        // len 域是 u32（wire keyed-SGL 24-bit / inline 32-bit，契约 §H#1）；> 4 GiB 的 MR 会在
        // mr_len 的 `as u32` 截断。锚定不变式供真实 IbverbsRdma 对齐（实际缓冲远小于此）。
        debug_assert!(bytes.len() <= u32::MAX as usize, "MR 长度溢出 u32 域");
        let handle = self.next_mr;
        self.next_mr += 1;
        self.mrs.insert(handle, Mr { bytes, access });
        Ok(MrHandle(handle))
    }

    fn mr_bytes(&self, mr: MrHandle) -> Option<&[u8]> {
        self.mrs.get(&mr.0).map(|m| m.bytes.as_slice())
    }

    fn mr_bytes_mut(&mut self, mr: MrHandle) -> Option<&mut [u8]> {
        self.mrs.get_mut(&mr.0).map(|m| m.bytes.as_mut_slice())
    }

    fn post_recv(&mut self, mr: MrHandle, wr_id: WrId) -> Result<(), RdmaError> {
        self.ensure_ready()?;
        if self.mr_len(mr).is_none() {
            return Err(RdmaError::BadMr(mr));
        }
        self.recv_bufs.push_back((mr, wr_id));
        Ok(())
    }

    fn post_send(
        &mut self,
        mr: MrHandle,
        len: u32,
        wr_id: WrId,
        _invalidate: Option<Rkey>,
    ) -> Result<(), RdmaError> {
        self.ensure_ready()?;
        self.check_local(mr, 0, len)?;
        let forced_error = self.take_forced_error();
        self.work.push_back(PendingWr {
            wr_id,
            op: PendingOp::Send,
            forced_error,
        });
        Ok(())
    }

    fn post_rdma_read(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
        wr_id: WrId,
    ) -> Result<(), RdmaError> {
        self.ensure_ready()?;
        self.check_local(local, local_off, len)?;
        if self.outstanding_reads >= self.ord_max {
            return Err(RdmaError::OrdExceeded { max: self.ord_max });
        }
        self.outstanding_reads += 1;
        let forced_error = self.take_forced_error();
        self.work.push_back(PendingWr {
            wr_id,
            op: PendingOp::RdmaRead {
                local,
                local_off,
                remote_addr,
                rkey,
                len,
            },
            forced_error,
        });
        Ok(())
    }

    fn post_rdma_write(
        &mut self,
        local: MrHandle,
        local_off: u32,
        remote_addr: u64,
        rkey: Rkey,
        len: u32,
        wr_id: WrId,
    ) -> Result<(), RdmaError> {
        self.ensure_ready()?;
        self.check_local(local, local_off, len)?;
        let forced_error = self.take_forced_error();
        self.work.push_back(PendingWr {
            wr_id,
            op: PendingOp::RdmaWrite {
                local,
                local_off,
                remote_addr,
                rkey,
                len,
            },
            forced_error,
        });
        Ok(())
    }

    fn poll_cq(&mut self) -> Vec<Completion> {
        let mut out = Vec::new();
        // **已知简化**（§D.1 边界）：本 mock 在一次 poll 内总是先 drain recv completion、再处理
        // send-side work。真 CQ 按到达序交错两类完成（一个先 post 的 SEND 可能先于后到的 Recv
        // 完成）。target 侧 recv/send-side 跨两队列本就弱序，此简化可接受——但 R4 **不可**依赖
        // 「一次 poll 内 Recv 必先于 send-side」。
        out.extend(self.recv_done.drain(..));
        // 再按提交顺序物化 send-side work（§D.1 保序）。
        while let Some(wr) = self.work.pop_front() {
            if self.qp == QpState::Error {
                // QP 已 error：本 WR + 队列其余全 flush（§D.2）。
                if matches!(wr.op, PendingOp::RdmaRead { .. }) {
                    self.outstanding_reads = self.outstanding_reads.saturating_sub(1);
                }
                let opcode = match wr.op {
                    PendingOp::Send => Opcode::Send,
                    PendingOp::RdmaRead { .. } => Opcode::RdmaRead,
                    PendingOp::RdmaWrite { .. } => Opcode::RdmaWrite,
                };
                out.push(Completion {
                    wr_id: wr.wr_id,
                    opcode,
                    status: WcStatus::Error(WcError::WrFlushError),
                    byte_len: 0,
                    invalidated_rkey: None,
                });
                continue;
            }
            let comp = self.execute(wr);
            let errored = matches!(comp.status, WcStatus::Error(_));
            out.push(comp);
            if errored {
                // 一个 WR 出错 → QP 进 error；本次 poll 剩余 WR 在下一轮循环走 flush 分支。
                self.qp = QpState::Error;
            }
        }
        // QP 进 error 后，**posted-but-unconsumed recv WR 也 flush**（真 QP→ERR 语义，§D.2）。
        // flush 后 recv_bufs 清空 → 后续 inject_send 撞 RNR，逼 target 重建后重新 post_recv
        // （M1 修复：否则 mock 留着 recv buffer 会教 R3/R4 错误的 recv-reposting 模型）。
        if self.qp == QpState::Error {
            for (_, wr_id) in self.recv_bufs.drain(..) {
                out.push(Completion {
                    wr_id,
                    opcode: Opcode::Recv,
                    status: WcStatus::Error(WcError::WrFlushError),
                    byte_len: 0,
                    invalidated_rkey: None,
                });
            }
        }
        out
    }

    fn qp_state(&self) -> QpState {
        self.qp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RKEY: Rkey = Rkey(0xABCD);
    const REMOTE_BASE: u64 = 0x1_0000_0000; // 高地址（>4 GiB，对齐项目 §26 子条款意图）

    /// hello-QP smoke：recv 一个对端 SEND；RDMA WRITE 推数据到 host；RDMA READ 拉数据回。
    #[test]
    fn hello_qp_smoke() {
        let mut q = MockRdma::new(4);
        // target 注册一块本地 recv buffer。
        let recv_mr = q.reg_mr(vec![0u8; 64], Access::local_write()).unwrap();
        q.post_recv(recv_mr, 0x100).unwrap();

        // 对端 host SEND 一个 64B 命令胶囊。
        let capsule: Vec<u8> = (0..64).collect();
        q.inject_send(&capsule, None).unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].opcode, Opcode::Recv);
        assert_eq!(comps[0].status, WcStatus::Success);
        assert_eq!(comps[0].byte_len, 64);
        assert_eq!(q.mr_bytes(recv_mr).unwrap(), capsule.as_slice());

        // host 暴露一块远端 MR（读命令的目标缓冲）。
        q.stub_remote(RKEY, REMOTE_BASE, vec![0u8; 32]);
        // target RDMA WRITE：把本地数据推到 host（读命令送数据）。
        let src = q
            .reg_mr((100..132).collect(), Access::local_read())
            .unwrap();
        q.post_rdma_write(src, 0, REMOTE_BASE, RKEY, 32, 0x200)
            .unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].opcode, Opcode::RdmaWrite);
        assert_eq!(comps[0].status, WcStatus::Success);
        assert_eq!(
            q.remote_bytes(RKEY).unwrap(),
            (100u8..132).collect::<Vec<_>>().as_slice()
        );

        // target RDMA READ：从 host 拉数据回本地（写命令取数据）。
        let dst = q.reg_mr(vec![0u8; 32], Access::local_write()).unwrap();
        q.post_rdma_read(dst, 0, REMOTE_BASE, RKEY, 32, 0x300)
            .unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps[0].opcode, Opcode::RdmaRead);
        assert_eq!(comps[0].status, WcStatus::Success);
        assert_eq!(
            q.mr_bytes(dst).unwrap(),
            (100u8..132).collect::<Vec<_>>().as_slice()
        );
    }

    /// §D.1 completion 保序：RDMA WRITE 链在 SEND 前 → 完成顺序 WRITE 先于 SEND。
    #[test]
    fn completion_ordering_write_before_send() {
        let mut q = MockRdma::new(4);
        q.stub_remote(RKEY, REMOTE_BASE, vec![0u8; 16]);
        let data = q.reg_mr(vec![7u8; 16], Access::local_read()).unwrap();
        let resp = q.reg_mr(vec![0u8; 16], Access::local_read()).unwrap();
        // 先 post WRITE 后 post SEND（target 读命令：先推数据、再发响应胶囊）。
        q.post_rdma_write(data, 0, REMOTE_BASE, RKEY, 16, 0xD1)
            .unwrap();
        q.post_send(resp, 16, 0x5E, None).unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].opcode, Opcode::RdmaWrite, "WRITE 必先于 SEND 完成");
        assert_eq!(comps[1].opcode, Opcode::Send);
    }

    /// §D.2：RDMA READ 撞未知 rkey → RemoteAccessError → QP 进 error → 后续 in-flight flush。
    #[test]
    fn qp_error_on_bad_rkey_flushes_inflight() {
        let mut q = MockRdma::new(4);
        let dst = q.reg_mr(vec![0u8; 16], Access::local_write()).unwrap();
        let resp = q.reg_mr(vec![0u8; 16], Access::local_read()).unwrap();
        // READ 一个没 stub 的 rkey → 出错；同 poll 内后续 SEND 应 flush。
        q.post_rdma_read(dst, 0, REMOTE_BASE, Rkey(0xDEAD), 16, 0xDD)
            .unwrap();
        q.post_send(resp, 16, 0x5E, None).unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].status, WcStatus::Error(WcError::RemoteAccessError));
        assert_eq!(
            comps[1].status,
            WcStatus::Error(WcError::WrFlushError),
            "QP error 后 in-flight WR 必 flush"
        );
        assert_eq!(q.qp_state(), QpState::Error);
        // QP error 态新 post 被拒。
        assert_eq!(q.post_send(resp, 16, 1, None), Err(RdmaError::QpInError));
        // 重建后恢复可用。
        q.recreate_qp();
        assert_eq!(q.qp_state(), QpState::Ready);
        assert!(q.post_send(resp, 16, 2, None).is_ok());
    }

    /// §D.2 注入错误：inject_wc_error 强制下一个 WR 失败 → QP error。
    #[test]
    fn injected_wc_error_trips_qp() {
        let mut q = MockRdma::new(4);
        let resp = q.reg_mr(vec![0u8; 16], Access::local_read()).unwrap();
        q.inject_wc_error(WcError::RnrRetryExceeded);
        q.post_send(resp, 16, 0x1, None).unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps[0].status, WcStatus::Error(WcError::RnrRetryExceeded));
        assert_eq!(q.qp_state(), QpState::Error);
    }

    /// §D.3 RNR：对端 SEND 撞无 posted recv buffer。
    #[test]
    fn rnr_when_no_recv_buffer() {
        let mut q = MockRdma::new(4);
        assert_eq!(q.inject_send(b"cmd", None), Err(WcError::RnrRetryExceeded));
        // post 一个 recv 后即可收。
        let mr = q.reg_mr(vec![0u8; 8], Access::local_write()).unwrap();
        q.post_recv(mr, 0x1).unwrap();
        assert!(q.inject_send(b"cmd", None).is_ok());
    }

    /// §D.4 ORD：in-flight RDMA READ 超 ord_max → OrdExceeded；poll 完成后腾出额度。
    #[test]
    fn ord_depth_limits_concurrent_reads() {
        let mut q = MockRdma::new(2); // ORD=2
        q.stub_remote(RKEY, REMOTE_BASE, vec![0u8; 64]);
        let dst = q.reg_mr(vec![0u8; 64], Access::local_write()).unwrap();
        q.post_rdma_read(dst, 0, REMOTE_BASE, RKEY, 8, 1).unwrap();
        q.post_rdma_read(dst, 8, REMOTE_BASE, RKEY, 8, 2).unwrap();
        // 第 3 个超 ORD。
        assert_eq!(
            q.post_rdma_read(dst, 16, REMOTE_BASE, RKEY, 8, 3),
            Err(RdmaError::OrdExceeded { max: 2 })
        );
        // poll 完成前两个，腾出额度后可再 post。
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 2);
        assert!(q.post_rdma_read(dst, 16, REMOTE_BASE, RKEY, 8, 3).is_ok());
    }

    /// SEND_WITH_INV：inject_send 带 invalidate → Recv completion 带回 rkey（§B.4）。
    #[test]
    fn recv_carries_invalidate_rkey() {
        let mut q = MockRdma::new(4);
        let mr = q.reg_mr(vec![0u8; 8], Access::local_write()).unwrap();
        q.post_recv(mr, 0x9).unwrap();
        q.inject_send(b"cmd", Some(RKEY)).unwrap();
        let comps = q.poll_cq();
        assert_eq!(comps[0].invalidated_rkey, Some(RKEY));
    }

    /// 本地 MR 越界 → OutOfBounds（同步拒，post 期）。
    #[test]
    fn local_mr_bounds_checked() {
        let mut q = MockRdma::new(4);
        let mr = q.reg_mr(vec![0u8; 8], Access::local_read()).unwrap();
        assert_eq!(
            q.post_send(mr, 16, 1, None),
            Err(RdmaError::OutOfBounds {
                off: 0,
                len: 16,
                mr_len: 8
            })
        );
    }

    /// §D.4 回归守卫：error+flush 路径必须各退回 ORD 额度（防计数漂移 latent 失效）。
    /// post 2 READ（ORD=2 满），第 1 个注错 → 第 2 个 flush；两者各 -1；重建后能再 post 2 个。
    #[test]
    fn ord_credit_returned_on_error_and_flush() {
        let mut q = MockRdma::new(2);
        q.stub_remote(RKEY, REMOTE_BASE, vec![0u8; 64]);
        let dst = q.reg_mr(vec![0u8; 64], Access::local_write()).unwrap();
        q.inject_wc_error(WcError::RemoteAccessError); // 作用于下一个 post（第 1 个 READ）
        q.post_rdma_read(dst, 0, REMOTE_BASE, RKEY, 8, 1).unwrap();
        q.post_rdma_read(dst, 8, REMOTE_BASE, RKEY, 8, 2).unwrap(); // 满 ORD
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].status, WcStatus::Error(WcError::RemoteAccessError)); // 第 1 个注错
        assert_eq!(comps[1].status, WcStatus::Error(WcError::WrFlushError)); // 第 2 个 flush
        q.recreate_qp();
        // 若任一路径漏退额度，这里第 2 个 post 会误报 OrdExceeded。
        assert!(q.post_rdma_read(dst, 0, REMOTE_BASE, RKEY, 8, 3).is_ok());
        assert!(q.post_rdma_read(dst, 8, REMOTE_BASE, RKEY, 8, 4).is_ok());
    }

    /// §D.2 跨 poll：error 在 poll N 发生，poll N+1 新 post 仍被拒，recreate 后跨 poll 边界恢复。
    #[test]
    fn qp_error_persists_across_polls_until_recreate() {
        let mut q = MockRdma::new(4);
        let resp = q.reg_mr(vec![0u8; 16], Access::local_read()).unwrap();
        q.inject_wc_error(WcError::RnrRetryExceeded);
        q.post_send(resp, 16, 1, None).unwrap();
        let _ = q.poll_cq(); // poll N：error 物化，QP→Error
        assert_eq!(q.qp_state(), QpState::Error);
        // poll N+1：新 post 仍拒（跨 poll error 持续）。
        assert_eq!(q.post_send(resp, 16, 2, None), Err(RdmaError::QpInError));
        assert!(q.poll_cq().is_empty());
        q.recreate_qp();
        assert!(q.post_send(resp, 16, 3, None).is_ok());
    }

    /// M1 守卫：QP→ERR 时 posted-but-unconsumed recv WR 也 flush；flush 后 recv 不可再消费
    /// （逼 target 重建后重新 post_recv，对齐真 verbs）。
    #[test]
    fn posted_recv_flushed_on_qp_error() {
        let mut q = MockRdma::new(4);
        let recv_mr = q.reg_mr(vec![0u8; 16], Access::local_write()).unwrap();
        let resp = q.reg_mr(vec![0u8; 16], Access::local_read()).unwrap();
        q.post_recv(recv_mr, 0xAA).unwrap(); // 先 post 一个 recv
        q.inject_wc_error(WcError::RemoteAccessError);
        q.post_send(resp, 16, 0x5E, None).unwrap();
        let comps = q.poll_cq();
        // SEND 出错 + posted recv 被 flush。
        assert!(
            comps.iter().any(|c| c.opcode == Opcode::Send
                && c.status == WcStatus::Error(WcError::RemoteAccessError))
        );
        assert!(comps.iter().any(|c| c.wr_id == 0xAA
            && c.opcode == Opcode::Recv
            && c.status == WcStatus::Error(WcError::WrFlushError)));
        q.recreate_qp();
        // recv buffer 已 flush → 现在 inject_send 撞 RNR，须重新 post_recv。
        assert_eq!(q.inject_send(b"cmd", None), Err(WcError::RnrRetryExceeded));
        q.post_recv(recv_mr, 0xBB).unwrap();
        assert!(q.inject_send(b"cmd", None).is_ok());
    }

    /// R4 cumulative-offset 路径：多 fragment 顺序推进 local_off + remote_addr，各段独立落地。
    #[test]
    fn multi_fragment_cumulative_offset() {
        let mut q = MockRdma::new(8);
        q.stub_remote(RKEY, REMOTE_BASE, vec![0u8; 24]);
        let src = q.reg_mr((0u8..24).collect(), Access::local_read()).unwrap();
        // 3 段 ×8 字节，cumulative offset 推进（桥按 token-FIFO + cumulative offset 切段，§E）。
        for (i, off) in [0u32, 8, 16].into_iter().enumerate() {
            q.post_rdma_write(src, off, REMOTE_BASE + off as u64, RKEY, 8, i as u64)
                .unwrap();
        }
        let comps = q.poll_cq();
        assert_eq!(comps.len(), 3);
        assert!(comps.iter().all(|c| c.status == WcStatus::Success));
        // 三段拼回完整 0..24。
        assert_eq!(
            q.remote_bytes(RKEY).unwrap(),
            (0u8..24).collect::<Vec<_>>().as_slice()
        );
    }
}
