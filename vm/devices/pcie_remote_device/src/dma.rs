// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DMA helpers — GPA 范围校验 (spec §3.7)。
//!
//! **不提供** IOMMU 等价隔离；仅做：
//! - 单次长度 ≤ MAX_DMA_BYTES
//! - gpa+len ≤ ram_end 的硬上限
//! - gpa+len 溢出检查
//!
//! 调用方负责传入 `gm.vtl0()` 视图（不含 VTL2 私有页）。

use anyhow::Result;
use anyhow::anyhow;
use guestmem::GuestMemory;
use pcie_remote_protocol::MAX_DMA_BYTES;

/// 从 guest 物理地址读出最多 `len` 字节。
pub fn read_gpa(gm: &GuestMemory, gpa: u64, len: u32, ram_end: u64) -> Result<Vec<u8>> {
    if len as usize > MAX_DMA_BYTES {
        return Err(anyhow!("DMA read len {len} > MAX_DMA_BYTES"));
    }
    let end = gpa
        .checked_add(len as u64)
        .ok_or_else(|| anyhow!("gpa+len overflow"))?;
    if end > ram_end {
        return Err(anyhow!("gpa {gpa}+{len} exceeds ram_end {ram_end}"));
    }
    let mut buf = vec![0u8; len as usize];
    gm.read_at(gpa, &mut buf).map_err(|e| anyhow!("{e}"))?;
    Ok(buf)
}

/// 写入 guest 物理地址。
pub fn write_gpa(gm: &GuestMemory, gpa: u64, data: &[u8], ram_end: u64) -> Result<()> {
    if data.len() > MAX_DMA_BYTES {
        return Err(anyhow!("DMA write len {} > MAX_DMA_BYTES", data.len()));
    }
    let end = gpa
        .checked_add(data.len() as u64)
        .ok_or_else(|| anyhow!("gpa+len overflow"))?;
    if end > ram_end {
        return Err(anyhow!(
            "gpa {gpa}+{} exceeds ram_end {ram_end}",
            data.len()
        ));
    }
    gm.write_at(gpa, data).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_rejects_oversized() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, 0, (MAX_DMA_BYTES + 1) as u32, u64::MAX);
        assert!(r.is_err());
    }

    #[test]
    fn read_rejects_out_of_range() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, 1000, 100, 999);
        assert!(r.is_err());
    }

    #[test]
    fn read_rejects_overflow() {
        let gm = GuestMemory::empty();
        let r = read_gpa(&gm, u64::MAX, 1, u64::MAX);
        assert!(r.is_err());
    }

    #[test]
    fn write_rejects_oversized() {
        let gm = GuestMemory::empty();
        let buf = vec![0u8; MAX_DMA_BYTES + 1];
        let r = write_gpa(&gm, 0, &buf, u64::MAX);
        assert!(r.is_err());
    }
}
