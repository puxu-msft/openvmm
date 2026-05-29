// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Application-level handshake (spec §3.3 Connecting)。

use crate::Error;
use crate::prepared::PreparedPcieRemoteDevice;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
use mesh::channel;
use pcie_remote_protocol::DeviceDescribe;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::PROTOCOL_VERSION;
use pcie_remote_protocol::codec;
use zerocopy::IntoBytes;

/// PCIe ext-cap 区累加长度上限（与 ConfigSpaceCommonHeaderEmulator 对齐）。
const EXT_CAP_TOTAL_LIMIT: usize = 0xFFC;
/// MSI-X 表项数上限（与 MsixEmulator 对齐）。
const MSIX_COUNT_LIMIT: u32 = 2048;

/// 在 connected transport 上完成应用层握手。
///
/// 调用方负责在外层加超时（如 `CancelContext::with_timeout`）。
pub async fn handshake<T>(
    mut transport: T,
    instance_id: guid::Guid,
) -> Result<(PreparedPcieRemoteDevice, T), Error>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let hello = Hello {
        magic: PROTOCOL_MAGIC,
        version: PROTOCOL_VERSION,
        instance_id: instance_id.as_bytes().to_vec(),
    };
    codec::write_frame(&mut transport, &hello).await?;
    let ack: HelloAck = codec::read_frame(&mut transport).await?;
    if !ack.ok {
        return Err(Error::BarLayout(format!("host rejected: {}", ack.reason)));
    }
    let describe = ack
        .device
        .ok_or_else(|| Error::BarLayout("HelloAck.device missing".into()))?;
    validate_describe(&describe)?;
    let (sender, receiver) = channel::<crate::worker::DeviceRequest>();
    let prepared = PreparedPcieRemoteDevice {
        describe,
        to_worker: sender,
        worker_inbox: Some(receiver),
    };
    Ok((prepared, transport))
}

/// 校验 HelloAck 中的 DeviceDescribe，让非法 schema 在握手期就被拦截，
/// 避免之后 ConfigSpaceCommonHeaderEmulator 内的 assert 把 VM panic 掉。
fn validate_describe(d: &DeviceDescribe) -> Result<(), Error> {
    if d.msix_count > MSIX_COUNT_LIMIT {
        return Err(Error::MsixCountTooLarge(d.msix_count));
    }
    for bar in &d.bars {
        if bar.size == 0 || (bar.size & (bar.size - 1)) != 0 {
            return Err(Error::BarLayout(format!(
                "BAR {} size {} not a power of 2",
                bar.index, bar.size
            )));
        }
        if bar.size < 4096 {
            return Err(Error::BarLayout(format!(
                "BAR {} size {} < 4096",
                bar.index, bar.size
            )));
        }
    }
    let mut total = 0usize;
    for cap in &d.capabilities {
        if !cap.raw.len().is_multiple_of(4) {
            return Err(Error::CapabilityBlob(format!(
                "cap {} raw len {} not 4-aligned",
                cap.cap_id,
                cap.raw.len()
            )));
        }
        total += cap.raw.len() + 4;
        if total > EXT_CAP_TOTAL_LIMIT {
            return Err(Error::CapabilityBlob(format!(
                "cap total {total} > limit {EXT_CAP_TOTAL_LIMIT}",
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcie_remote_protocol::BarInfo;
    use pcie_remote_protocol::CapabilityBlob;
    use pcie_remote_protocol::bar_info::Kind;

    fn good_describe() -> DeviceDescribe {
        DeviceDescribe {
            vendor_id: 0x1414,
            device_id: 0xc000,
            class_code: 0x010802,
            revision: 1,
            subsystem_vendor: 0,
            subsystem_device: 0,
            bars: vec![BarInfo {
                index: 0,
                size: 4096,
                kind: Kind::Mmio32 as i32,
                prefetchable: false,
            }],
            msix_count: 1,
            capabilities: vec![],
            cfg_write_side_effect_offsets: vec![],
        }
    }

    #[test]
    fn rejects_bad_bar_size() {
        let mut d = good_describe();
        d.bars[0].size = 100;
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 0;
        assert!(validate_describe(&d).is_err());
        let mut d = good_describe();
        d.bars[0].size = 2048;
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn rejects_unaligned_cap_raw() {
        let mut d = good_describe();
        d.capabilities.push(CapabilityBlob {
            cap_id: 0x10,
            raw: vec![0u8; 7],
        });
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn rejects_msix_overflow() {
        let mut d = good_describe();
        d.msix_count = 4000;
        assert!(validate_describe(&d).is_err());
    }

    #[test]
    fn accepts_good_describe() {
        assert!(validate_describe(&good_describe()).is_ok());
    }
}
