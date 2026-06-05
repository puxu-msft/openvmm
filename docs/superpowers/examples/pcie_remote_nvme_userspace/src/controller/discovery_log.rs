// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase V7** — Discovery Log Page builder（spec § 5.16.1.20 Figure 350）。
//!
//! Discovery Log Page (LID=0x70) 用于 NVMe-oF Discovery Controller 把可
//! `Connect` 的 target NQN + 网络 portal 列表暴露给 host。Linux nvme-cli
//! `nvme discover -t tcp -a <ip> -s 4420` 收到的就是本 builder 输出的 byte
//! sequence。
//!
//! ## Wire format（spec § 5.16.1.20 Figure 350）
//!
//! ```text
//! 0000..0007  GENCTR   u64 LE  generation counter（每次 portal 集合变就 ++）
//! 0008..000f  NUMREC   u64 LE  number of records (entries)
//! 0010..0011  RECFMT   u16 LE  record format (0=current)
//! 0012..03ff           [u8]   reserved (zero)
//! 0400..07ff  entry[0] 1024 byte
//! 0800..0bff  entry[1] 1024 byte
//! ...
//! ```
//!
//! ## Entry format（每 1024 byte，Figure 351）
//!
//! ```text
//! 00..00      TRTYPE     u8     transport type (3 = TCP, spec § 5.16.1.20)
//! 01..01      ADRFAM     u8     address family (1 = IPv4, 2 = IPv6, 0xfe = FC)
//! 02..02      SUBTYPE    u8     subsystem type (1 = Discovery, 2 = NVM)
//! 03..03      TREQ       u8     transport requirements bitmap
//! 04..05      PORTID     u16 LE
//! 06..07      CNTLID     u16 LE
//! 08..09      ASQSZ      u16 LE  admin SQ size 推荐值（0 = default）
//! 0a..1f               [u8]   reserved
//! 20..3f      TRSVCID    [u8;32] ASCII service id（端口号字符串）
//! 40..ff               [u8]   reserved
//! 100..1ff    SUBNQN     [u8;256] ASCII NQN（trailing zero-pad）
//! 200..2ff    TRADDR     [u8;256] ASCII transport address（IP 字符串）
//! 300..3ff    TSAS       [u8;256] transport-specific address subtype
//! ```
//!
//! ## 教学限制
//!
//! - 单 entry 始终 1024 byte（spec 强制）
//! - 当前最多支持 ≤ 7 portals (8 KiB total / 1024 = 8 - header 1024 = 7)
//!   超出 controller `admin.rs` Get Log Page > 8 KiB reject (no PRP list yet)
//! - 仅 TCP transport (TRTYPE=3)；ADRFAM 仅 IPv4/IPv6 按 portal IP 自检
//! - TSAS 全 0（spec § 5.16.1.20 TCP 子段当前无强制内容）

use zerocopy::{Immutable, IntoBytes, KnownLayout};

/// 单个 Discovery Log entry（spec § 5.16.1.20 Figure 351）。1024 byte。
#[derive(Debug, Clone, Copy, IntoBytes, KnownLayout, Immutable)]
#[repr(C, packed)]
pub struct DiscoveryEntry {
    /// transport type (3 = TCP)
    pub trtype: u8,
    /// address family (1 = IPv4, 2 = IPv6)
    pub adrfam: u8,
    /// subsystem type (1 = Discovery, 2 = NVM)
    pub subtype: u8,
    /// transport requirements bitmap (bit 0 = secure channel required)
    pub treq: u8,
    /// portid
    pub portid: u16,
    /// cntlid
    pub cntlid: u16,
    /// admin SQ size 推荐值
    pub asqsz: u16,
    pub rsvd0: [u8; 22],
    /// ASCII service id（端口号字符串，trailing zero-pad）
    pub trsvcid: [u8; 32],
    pub rsvd1: [u8; 192],
    /// ASCII subsystem NQN
    pub subnqn: [u8; 256],
    /// ASCII transport address (IP 字符串)
    pub traddr: [u8; 256],
    /// transport-specific address subtype
    pub tsas: [u8; 256],
}

const _: () = assert!(core::mem::size_of::<DiscoveryEntry>() == 1024);

impl Default for DiscoveryEntry {
    fn default() -> Self {
        Self {
            trtype: 0,
            adrfam: 0,
            subtype: 0,
            treq: 0,
            portid: 0,
            cntlid: 0,
            asqsz: 0,
            rsvd0: [0u8; 22],
            trsvcid: [0u8; 32],
            rsvd1: [0u8; 192],
            subnqn: [0u8; 256],
            traddr: [0u8; 256],
            tsas: [0u8; 256],
        }
    }
}

/// 单个 Discovery target portal。bin 启动时 `--discovery-target-*` 转入。
#[derive(Debug, Clone)]
pub struct DiscoveryPortal {
    /// target NQN
    pub nqn: String,
    /// transport address (IP)
    pub traddr: String,
    /// transport service id (port 字符串)
    pub trsvcid: String,
    /// ADRFAM (1=IPv4, 2=IPv6)
    pub adrfam: u8,
    /// PORTID
    pub portid: u16,
}

impl DiscoveryPortal {
    /// Helper：从 "127.0.0.1:4421" 解析 IPv4 + port。
    pub fn from_ipv4_addr(nqn: impl Into<String>, addr: &str) -> anyhow::Result<Self> {
        let (ip, port) = addr
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("discovery portal addr {addr:?} 缺 ':port'"))?;
        Ok(Self {
            nqn: nqn.into(),
            traddr: ip.to_string(),
            trsvcid: port.to_string(),
            adrfam: 1, // IPv4
            portid: 1,
        })
    }
}

/// 构造 Discovery Log Page (spec § 5.16.1.20 Figure 350)。
///
/// `bytes` = host 通过 NUMD 请求的字节数；若小于 header+entries 的实际总长，
/// **截断到 bytes**（spec 允许）；若大于实际长，**zero-pad** 到 bytes。
///
/// `gen_ctr` = generation counter；每次 portals 集合变化 caller 该 ++。
pub fn build_discovery_log(gen_ctr: u64, portals: &[DiscoveryPortal], bytes: usize) -> Vec<u8> {
    let header_size = 1024usize;
    let entry_size = 1024usize;
    let total_size = header_size + portals.len() * entry_size;
    let mut buf = vec![0u8; total_size.max(bytes)];

    // ─── Header (前 1024 byte) ────────────────────────────────────────
    buf[0..8].copy_from_slice(&gen_ctr.to_le_bytes());
    buf[8..16].copy_from_slice(&(portals.len() as u64).to_le_bytes());
    buf[16..18].copy_from_slice(&0u16.to_le_bytes()); // RECFMT=0
    // bytes 18..1024 保留全 0

    // ─── Entries ────────────────────────────────────────────────────
    for (idx, p) in portals.iter().enumerate() {
        let mut entry = DiscoveryEntry {
            trtype: 3, // TCP
            adrfam: p.adrfam,
            subtype: 2, // NVM subsystem
            treq: 0,    // no secure channel
            portid: p.portid,
            cntlid: 0xFFFF, // dynamic
            asqsz: 32,
            ..Default::default()
        };
        copy_ascii(&mut entry.trsvcid, p.trsvcid.as_bytes());
        copy_ascii(&mut entry.subnqn, p.nqn.as_bytes());
        copy_ascii(&mut entry.traddr, p.traddr.as_bytes());
        let entry_offset = header_size + idx * entry_size;
        // tsas 已默认 0
        buf[entry_offset..entry_offset + entry_size].copy_from_slice(entry.as_bytes());
    }

    // 按 host 请求的 bytes 截断或 zero-pad
    buf.truncate(bytes);
    if buf.len() < bytes {
        buf.resize(bytes, 0);
    }
    buf
}

fn copy_ascii(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    // 剩余字节保持 0（trailing zero-pad）
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **V7-1** — empty portals header byte-exact：GENCTR=0 / NUMREC=0 / RECFMT=0
    #[test]
    fn v7_discovery_log_header_byte_exact() {
        let buf = build_discovery_log(0, &[], 1024);
        assert_eq!(buf.len(), 1024);
        // GENCTR (u64 LE) = 0
        assert_eq!(&buf[0..8], &[0u8; 8]);
        // NUMREC (u64 LE) = 0
        assert_eq!(&buf[8..16], &[0u8; 8]);
        // RECFMT (u16 LE) = 0
        assert_eq!(&buf[16..18], &[0u8, 0u8]);
        // 剩 reserved 全 0
        assert!(buf[18..1024].iter().all(|&b| b == 0));
    }

    /// **V7-2** — 2 portals layout：NUMREC=2 + 2 entries 各 1024B；
    /// 字段 byte-exact 验证 (TRTYPE=3, ADRFAM=1, SUBTYPE=2)
    #[test]
    fn v7_discovery_log_two_entries_layout() {
        let portals = vec![
            DiscoveryPortal {
                nqn: "nqn.test1".into(),
                traddr: "127.0.0.1".into(),
                trsvcid: "4421".into(),
                adrfam: 1,
                portid: 1,
            },
            DiscoveryPortal {
                nqn: "nqn.test2".into(),
                traddr: "192.168.1.1".into(),
                trsvcid: "4422".into(),
                adrfam: 1,
                portid: 2,
            },
        ];
        let buf = build_discovery_log(42, &portals, 1024 + 1024 * 2);
        assert_eq!(buf.len(), 1024 + 1024 * 2);
        // header GENCTR
        assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 42);
        assert_eq!(u64::from_le_bytes(buf[8..16].try_into().unwrap()), 2);

        // entry[0] @ offset 1024
        assert_eq!(buf[1024], 3, "TRTYPE=3 (TCP)");
        assert_eq!(buf[1025], 1, "ADRFAM=1 (IPv4)");
        assert_eq!(buf[1026], 2, "SUBTYPE=2 (NVM)");
        assert_eq!(u16::from_le_bytes(buf[1028..1030].try_into().unwrap()), 1);
        // TRSVCID @ entry[0] + 0x20 = 1024+32 = 1056
        assert_eq!(&buf[1056..1056 + 4], b"4421");
        // SUBNQN @ entry[0] + 0x100 = 1024+256 = 1280
        assert_eq!(&buf[1280..1280 + 9], b"nqn.test1");
        // TRADDR @ entry[0] + 0x200 = 1024+512 = 1536
        assert_eq!(&buf[1536..1536 + 9], b"127.0.0.1");

        // entry[1] @ offset 2048
        assert_eq!(buf[2048], 3);
        assert_eq!(u16::from_le_bytes(buf[2052..2054].try_into().unwrap()), 2);
        assert_eq!(&buf[2048 + 256..2048 + 256 + 9], b"nqn.test2");
        assert_eq!(&buf[2048 + 512..2048 + 512 + 11], b"192.168.1.1");
    }

    /// **V7-3** — host 请求 NumDW 仅返 header 前 N byte（spec 允许截断）
    #[test]
    fn v7_discovery_log_truncates_to_requested_bytes() {
        let portals = vec![DiscoveryPortal {
            nqn: "nqn.test".into(),
            traddr: "127.0.0.1".into(),
            trsvcid: "4420".into(),
            adrfam: 1,
            portid: 1,
        }];
        // host 请求仅 16 byte（典型先 fetch header 看 NUMREC）
        let buf = build_discovery_log(99, &portals, 16);
        assert_eq!(buf.len(), 16);
        assert_eq!(u64::from_le_bytes(buf[0..8].try_into().unwrap()), 99);
        assert_eq!(u64::from_le_bytes(buf[8..16].try_into().unwrap()), 1);
    }

    /// **V7-4** — host 请求字节超实际 size → zero-pad
    #[test]
    fn v7_discovery_log_zero_pads_excess_request() {
        let buf = build_discovery_log(0, &[], 4096);
        assert_eq!(buf.len(), 4096);
        // 实际 size 仅 1024 header；后 3072 应全 0
        assert!(buf[1024..].iter().all(|&b| b == 0));
    }

    /// **V7-5** — IPv4 helper 解析
    #[test]
    fn v7_discovery_portal_from_ipv4_addr() {
        let p = DiscoveryPortal::from_ipv4_addr("nqn.foo", "10.0.0.1:4420").unwrap();
        assert_eq!(p.nqn, "nqn.foo");
        assert_eq!(p.traddr, "10.0.0.1");
        assert_eq!(p.trsvcid, "4420");
        assert_eq!(p.adrfam, 1);
    }

    /// **V7-6** — DiscoveryEntry struct size = 1024 byte (spec § 5.16.1.20 Figure 351)
    #[test]
    fn v7_discovery_entry_size_is_1024() {
        assert_eq!(core::mem::size_of::<DiscoveryEntry>(), 1024);
    }
}
