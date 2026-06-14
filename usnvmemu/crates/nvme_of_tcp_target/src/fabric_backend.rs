// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **V9 RDMA R2** — `FabricBackend` 出站数据移动抽象 + `HostBuf` 三态 + `TcpFabricBackend`。
//!
//! 把「controller 的 sentinel-DMA → fabric 数据移动」抽成 transport-无关的 trait，让 TCP 与
//! 未来 RDMA 共享上层桥 orchestration（`run_post_dispatch` 的 drain + cumulative-offset 逻辑）。
//! 设计依据 = R0 契约
//! [`docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md`](../docs/specs/2026-06-14-nvme-rdma-wire-and-verbs-contract.md)
//! §E（`HostBuf` 三态/segments 模型）+ §F；纠正②：数据移动**借出/填入 buffer，不返 owned `Vec`**
//! （零拷贝可表达，hot-path-perf-first）。
//!
//! 分工：
//! - **`TcpFabricBackend`**（本 phase）：owns TCP stream + ttag 分配 + MAXH2CDATA；
//!   `move_host_to_local` = R2T+H2CData 多步；`move_local_to_host` = C2HData；
//!   `send_response_capsule` = RSP PDU。从 `AsyncSession` 的叶子方法**迁入**（非复制）。
//! - **`RdmaFabricBackend`**（R3/R4）：owns `RdmaVerbs` QP；`move_host_to_local` = RDMA READ；
//!   `move_local_to_host` = RDMA WRITE；`send_response_capsule` = SEND。

use anyhow::Context as _;
use tokio::io::AsyncReadExt;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

use crate::async_session::AsyncSessionStream;
use crate::framing::{read_pdu_async, write_pdu_async};
use crate::pdu::{CommonHdr, DataPsh, pdu_type};

/// host 数据缓冲的位置——全 transport 形态（R0 §E，spec-complete）。
///
/// `TcpFabricBackend` 只用 `FlowControlled`；RDMA backend 用 `Keyed`+`Inline`。R0 在此定死字段
/// 使 R2 不盲定日后要改的类型（architect gate ⑤：covers TCP-无远端 / RDMA-keyed-SGL / inline 三态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostBuf {
    /// TCP：无远端句柄，靠 R2T/H2CData PDU 流控；只有逻辑长度。
    FlowControlled {
        /// 整段逻辑字节数。
        total_len: u32,
    },
    /// RDMA keyed-SGL：target 发起单边 RDMA READ(写命令)/WRITE(读命令)。
    /// `data` = 数据段（msdbd=1 即常见 fast-reg 单段；msdbd>1 = SGL list 多段）；
    /// `meta` = separate-meta PI 的独立 PI 段（`MSDS` 能力；inline-meta/无 PI 时 None）。
    Keyed {
        /// 数据段（≥1）。
        data: Vec<KeyedSeg>,
        /// 独立 PI 元数据段（separate-meta），否则 None。
        meta: Option<KeyedSeg>,
    },
    /// RDMA in-capsule inline：数据已在收到的 capsule buffer 内（icdoff 偏移）。
    Inline {
        /// 数据在 capsule buffer 内的偏移。
        capsule_offset: u32,
        /// 数据长度。
        len: u32,
    },
}

/// 一个 keyed 远端区域（R0 §B.2）。**不变式：`len ≤ 0xFF_FFFF`（24-bit）**，
/// 由解析入站 capsule SGL 的边界（parse-don't-validate）把关、超界 reject。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyedSeg {
    /// 远端虚拟地址 / MR iova。
    pub remote_addr: u64,
    /// 远端保护键（rkey）。
    pub rkey: u32,
    /// 长度（24-bit 值域）。
    pub len: u32,
}

/// 出站数据移动后端（TCP / RDMA 各 impl，**owns 自己的 transport handle**）。
///
/// 上层桥（`run_post_dispatch`）在 controller 锁外调本 trait 把 sentinel-捕获的 DMA 翻成 wire；
/// `cid` = NVMe command id（TCP 作 cccid / RDMA 关联 WR）。**所有数据搬运填入/借出 caller buffer，
/// 不返 owned `Vec`**（纠正②零拷贝）。
///
/// **分阶段债（architect review，R0 §E L195-199 已预告，非 R2 设计错误）**：
/// - **入站 recv 未进本 trait**：pump 的 `select!` 仍直借 `TcpFabricBackend::stream` 读 PDU。RDMA recv
///   是 `poll_cq`（非 read stream），**R3 入口须把 recv 抽成 `recv_next(&mut self)` trait 方法**
///   （TCP impl 内部 `read_pdu_async`、RDMA impl 内部 poll_cq），届时 `stream` 字段降回私有。
/// - **`move_*` 签名是 TCP 形状**：`data_offset`/`is_last` 是 C2HData chunking 语义；RDMA keyed-SGL 多段
///   需按桥的 cumulative-offset 二级寻址（O→某 `KeyedSeg`+段内偏移，R0 §E），`is_last` 在 RDMA 无对应
///   （由 CQE/SEND 表达）。**R4 做 `RdmaFabricBackend` 时预期调签名**（最可能：传已被桥定位好的单段+段内
///   偏移）。`HostBuf` 类型本身已 RDMA-complete（segments 模型），不需改；只 `move_*` 入参形状会演进。
/// - **桥层 CQE 分流仍 TCP-specific**：`run_post_dispatch` 靠 `CQ_BASE_GPA` 哨值二分 CQE-write vs data-write；
///   RDMA 下 CQE 走 SEND response capsule（非 dma_write 到 sentinel），R4 须在桥层再抽一层分流。
#[allow(async_fn_in_trait)] // 本 trait 仅被 crate 内具体类型实现/调用（非 dyn），Send 由具体 S 保证
pub trait FabricBackend {
    /// 发响应胶囊（16B CQE）。TCP：RSP PDU；RDMA：SEND（或 SEND_WITH_INV）。
    async fn send_response_capsule(&mut self, cqe: &[u8; 16]) -> anyhow::Result<()>;

    /// 读路径：把本地 `data` 移到 host（读命令 controller→host 数据）。
    /// TCP：C2HData PDU（`data_offset` 累计 host 偏移 + `is_last` 打 DATA_LAST）；RDMA：RDMA WRITE。
    async fn move_local_to_host(
        &mut self,
        cid: u16,
        host: &HostBuf,
        data: &[u8],
        data_offset: u32,
        is_last: bool,
    ) -> anyhow::Result<()>;

    /// 写路径：从 host 取数据**填入** `dst`（写命令 host→controller 数据）。
    /// TCP：R2T → 多段 H2CData 重组；RDMA：RDMA READ。`base_offset` = 本 fragment 在整条命令里的
    /// 累计偏移（桥的 cumulative offset）。
    async fn move_host_to_local(
        &mut self,
        cid: u16,
        host: &HostBuf,
        base_offset: u32,
        dst: &mut [u8],
    ) -> anyhow::Result<()>;
}

/// H2CData 读超时（host 半开/卡死兜底）。与迁入前 `async_session::H2C_DATA_READ_TIMEOUT` 等值。
const H2C_DATA_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// NVMe-oF **TCP** 的 `FabricBackend`：owns stream + ttag 分配 + 协商的 MAXH2CDATA。
///
/// 数据移动逻辑从 `AsyncSession` 的 `dma_read_via_r2t_async` / `send_c2h_data_at_async` /
/// `write_capsule_resp_bytes_async` / `await_host_data_async` **迁入**（byte-identical wire）。
pub struct TcpFabricBackend<S: AsyncSessionStream> {
    /// TCP 连接（pump 的 recv select! 也借此字段，故 `pub(crate)`）。
    /// **R3 落地条件**：recv 抽成 `FabricBackend::recv_next` trait 方法后，本字段降回私有
    /// （RDMA backend owns QP 无 stream 字段，select! recv arm 不能再直借具体字段）。
    pub(crate) stream: S,
    /// V4b R2T transfer tag 分配器（写路径 dma_read 用）。
    pub(crate) ttag_alloc: crate::TtagAllocator,
    /// ICResp 协商的 MAXH2CDATA（R2T 单片上限）。
    maxh2cdata: u32,
}

impl<S: AsyncSessionStream> TcpFabricBackend<S> {
    /// 新建。`maxh2cdata` = 握手协商值（[`crate::MAXH2CDATA_BYTES`]）。
    pub fn new(stream: S, maxh2cdata: u32) -> Self {
        Self {
            stream,
            ttag_alloc: crate::TtagAllocator::default(),
            maxh2cdata,
        }
    }

    /// 发 C2HTermReq（FES）。从 `send_c2h_term_async` 迁入。
    pub async fn send_c2h_term(&mut self, fes: u16) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_TERM,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        let mut psh = [0u8; 16];
        psh[0..2].copy_from_slice(&fes.to_le_bytes());
        write_pdu_async(&mut self.stream, &hdr, &psh, &[])
            .await
            .context("R2 async write C2HTermReq")
    }

    /// 单片 R2T 子路径：分配 ttag、发 R2T、await H2CData 重组。从 `dma_read_one_chunk_async` 迁入。
    async fn dma_read_one_chunk(
        &mut self,
        cid: u16,
        offset: u32,
        chunk: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let ttag = self.ttag_alloc.alloc();
        let (hdr, psh) = crate::r2t::encode_r2t(cid, ttag, offset, chunk);
        write_pdu_async(&mut self.stream, &hdr, psh.as_bytes(), &[])
            .await
            .context("R2 async write R2T PDU")?;
        await_host_data_async(&mut self.stream, cid, ttag, offset, chunk)
            .await
            .with_context(|| {
                format!("R2 await_host_data failed (ttag={ttag}, off={offset}, chunk={chunk})")
            })
    }
}

impl<S: AsyncSessionStream> FabricBackend for TcpFabricBackend<S> {
    async fn send_response_capsule(&mut self, cqe: &[u8; 16]) -> anyhow::Result<()> {
        let hdr = CommonHdr {
            pdu_type: pdu_type::RSP,
            flags: 0,
            hlen: 24,
            pdo: 0,
            plen: 24,
        };
        write_pdu_async(&mut self.stream, &hdr, cqe, &[])
            .await
            .context("R2 async write CapsuleResp")
    }

    async fn move_local_to_host(
        &mut self,
        cid: u16,
        host: &HostBuf,
        data: &[u8],
        data_offset: u32,
        is_last: bool,
    ) -> anyhow::Result<()> {
        debug_assert!(
            matches!(host, HostBuf::FlowControlled { .. }),
            "TcpFabricBackend 仅支持 FlowControlled（TCP 无 keyed-SGL）"
        );
        let _ = host;
        let flags = if is_last {
            crate::pdu::flags::DATA_LAST
        } else {
            0
        };
        let hdr = CommonHdr {
            pdu_type: pdu_type::C2H_DATA,
            flags,
            hlen: 24,
            pdo: 24,
            plen: 24 + data.len() as u32,
        };
        let psh = DataPsh {
            cccid: cid,
            ttag_or_rsvd: 0,
            data_offset,
            data_length: data.len() as u32,
            rsvd: [0u8; 4],
        };
        write_pdu_async(&mut self.stream, &hdr, psh.as_bytes(), data)
            .await
            .context("R2 async write C2HData")
    }

    async fn move_host_to_local(
        &mut self,
        cid: u16,
        host: &HostBuf,
        base_offset: u32,
        dst: &mut [u8],
    ) -> anyhow::Result<()> {
        debug_assert!(
            matches!(host, HostBuf::FlowControlled { .. }),
            "TcpFabricBackend 仅支持 FlowControlled（TCP 无 keyed-SGL）"
        );
        let _ = host;
        let len = dst.len() as u32;
        if len > crate::session::V4_MAX_DMA_READ_BYTES {
            anyhow::bail!(
                "R2 move_host_to_local len {len} exceeds cap {}",
                crate::session::V4_MAX_DMA_READ_BYTES
            );
        }
        let max = self.maxh2cdata;
        let mut filled: u32 = 0;
        while filled < len {
            let remaining = len - filled;
            let chunk = remaining.min(max);
            let cmd_offset = base_offset.saturating_add(filled);
            let bytes = self.dma_read_one_chunk(cid, cmd_offset, chunk).await?;
            debug_assert_eq!(bytes.len(), chunk as usize);
            dst[filled as usize..(filled + chunk) as usize].copy_from_slice(&bytes);
            filled += chunk;
        }
        Ok(())
    }
}

/// 接 R2T 后的多段 H2CData 重组（含 Windows PLEN=HLEN quirk）。从 `await_host_data_async` 迁入。
async fn await_host_data_async<S>(
    stream: &mut S,
    cid: u16,
    ttag: u16,
    base_offset: u32,
    expected_len: u32,
) -> anyhow::Result<Vec<u8>>
where
    S: AsyncSessionStream,
{
    let mut r = crate::H2cReassembler::with_base_offset(cid, ttag, base_offset, expected_len);
    loop {
        let mut pdu =
            match tokio::time::timeout(H2C_DATA_READ_TIMEOUT, read_pdu_async(stream)).await {
                Ok(res) => res.context("R2 read H2CData")?,
                Err(_) => anyhow::bail!("R2T host-data: read H2CData 超时（host 半开/卡死）"),
            };
        let pt = pdu.header.pdu_type;
        if pt == pdu_type::H2C_TERM {
            anyhow::bail!("R2 host sent H2CTermReq while awaiting H2CData");
        }
        // **Windows H2CData PLEN quirk（真 WS2025 互通）** — Windows 把 H2CData 的 PLEN 设为 HLEN
        // （仅头），data 长度由 PSH DATAL 给。按 DATAL 补读，上界 = R2T 剩余防 DoS。
        if pt == pdu_type::H2C_DATA
            && pdu.psh.len() >= 16
            && let Ok(psh) = DataPsh::read_from_bytes(&pdu.psh[..16])
        {
            let datal = psh.data_length as usize;
            let remaining = (expected_len as usize).saturating_sub(r.received() as usize);
            if pdu.data.len() < datal && datal <= remaining {
                let missing = datal - pdu.data.len();
                let mut extra = vec![0u8; missing];
                match tokio::time::timeout(H2C_DATA_READ_TIMEOUT, stream.read_exact(&mut extra))
                    .await
                {
                    Ok(res) => {
                        res.context("Windows H2CData trailing data (PLEN=HLEN quirk)")?;
                    }
                    Err(_) => {
                        anyhow::bail!("R2T host-data: trailing data 读超时（host 半开/卡死）")
                    }
                }
                pdu.data.extend_from_slice(&extra);
            }
        }
        match r.accept_pdu(&pdu) {
            crate::AcceptOutcome::Continue => continue,
            crate::AcceptOutcome::Done(bytes) => return Ok(bytes),
            crate::AcceptOutcome::Error { fes, reason } => {
                let term_hdr = CommonHdr {
                    pdu_type: pdu_type::C2H_TERM,
                    flags: 0,
                    hlen: 24,
                    pdo: 0,
                    plen: 24,
                };
                let mut term_psh = [0u8; 16];
                term_psh[0..2].copy_from_slice(&fes.to_le_bytes());
                let _ = write_pdu_async(stream, &term_hdr, &term_psh, &[]).await;
                anyhow::bail!("R2 H2CData reassembly: fes={fes:#x} reason={reason}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HostBuf 三态可构造（R2 gate：covers TCP / keyed / inline）。
    #[test]
    fn hostbuf_three_states_constructible() {
        let _tcp = HostBuf::FlowControlled { total_len: 4096 };
        let _keyed = HostBuf::Keyed {
            data: vec![KeyedSeg {
                remote_addr: 0x1_0000_0000,
                rkey: 0xABCD,
                len: 0xFF_FFFF,
            }],
            meta: Some(KeyedSeg {
                remote_addr: 0x2_0000_0000,
                rkey: 0x1234,
                len: 8,
            }),
        };
        let _inline = HostBuf::Inline {
            capsule_offset: 0,
            len: 512,
        };
    }

    /// send_response_capsule byte-exact：RSP PDU 头 24B + 16B CQE。
    #[tokio::test]
    async fn send_response_capsule_byte_exact() {
        let (a, mut b) = tokio::io::duplex(4096);
        let mut be = TcpFabricBackend::new(a, crate::MAXH2CDATA_BYTES);
        let mut cqe = [0u8; 16];
        cqe[12..14].copy_from_slice(&0x1234u16.to_le_bytes()); // cid
        be.send_response_capsule(&cqe).await.unwrap();
        let pdu = read_pdu_async(&mut b).await.unwrap();
        assert_eq!(pdu.header.pdu_type, pdu_type::RSP);
        let plen = pdu.header.plen;
        assert_eq!(plen, 24);
        assert_eq!(&pdu.psh[..16], &cqe);
    }

    /// move_local_to_host byte-exact：C2HData PDU 带 data_offset + DATA_LAST。
    #[tokio::test]
    async fn move_local_to_host_c2h_byte_exact() {
        let (a, mut b) = tokio::io::duplex(4096);
        let mut be = TcpFabricBackend::new(a, crate::MAXH2CDATA_BYTES);
        let data: Vec<u8> = (0..32).collect();
        let host = HostBuf::FlowControlled { total_len: 32 };
        be.move_local_to_host(
            0x55, &host, &data, /*data_offset*/ 8, /*is_last*/ true,
        )
        .await
        .unwrap();
        let pdu = read_pdu_async(&mut b).await.unwrap();
        assert_eq!(pdu.header.pdu_type, pdu_type::C2H_DATA);
        assert_eq!(
            pdu.header.flags & crate::pdu::flags::DATA_LAST,
            crate::pdu::flags::DATA_LAST
        );
        let psh = DataPsh::read_from_bytes(&pdu.psh[..16]).unwrap();
        let (cccid, data_offset, data_length) = (psh.cccid, psh.data_offset, psh.data_length);
        assert_eq!(cccid, 0x55);
        assert_eq!(data_offset, 8);
        assert_eq!(data_length, 32);
        assert_eq!(pdu.data, data);
    }

    /// move_host_to_local roundtrip：发 R2T、回 H2CData、dst 被填入。
    #[tokio::test]
    async fn move_host_to_local_r2t_roundtrip() {
        let (a, b) = tokio::io::duplex(8192);
        let mut be = TcpFabricBackend::new(a, crate::MAXH2CDATA_BYTES);
        // host 侧任务：收 R2T → 回单片 H2CData（DATA_LAST）。
        let host_task = tokio::spawn(async move {
            let mut b = b;
            let r2t = read_pdu_async(&mut b).await.unwrap();
            assert_eq!(r2t.header.pdu_type, pdu_type::R2T);
            // 解 R2T PSH 拿 ttag/offset/len。
            let r2t_psh = crate::pdu::R2tPsh::read_from_bytes(&r2t.psh[..16]).unwrap();
            let payload: Vec<u8> = (0..r2t_psh.r2t_length as u8).collect();
            let hdr = CommonHdr {
                pdu_type: pdu_type::H2C_DATA,
                flags: crate::pdu::flags::DATA_LAST,
                hlen: 24,
                pdo: 24,
                plen: 24 + payload.len() as u32,
            };
            let psh = DataPsh {
                cccid: r2t_psh.cccid,
                ttag_or_rsvd: r2t_psh.ttag,
                data_offset: r2t_psh.r2t_offset,
                data_length: payload.len() as u32,
                rsvd: [0u8; 4],
            };
            write_pdu_async(&mut b, &hdr, psh.as_bytes(), &payload)
                .await
                .unwrap();
        });
        let host = HostBuf::FlowControlled { total_len: 16 };
        let mut dst = vec![0u8; 16];
        be.move_host_to_local(0x77, &host, /*base_offset*/ 0, &mut dst)
            .await
            .unwrap();
        host_task.await.unwrap();
        assert_eq!(dst, (0u8..16).collect::<Vec<_>>());
    }
}
