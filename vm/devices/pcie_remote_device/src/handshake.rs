// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Application-level handshake (spec §3.3 Connecting)。
//!
//! v2 重构后：handshake 只完成 wire-level 协议握手 + describe 校验，
//! 不构造 channel 不构造 PreparedPcieRemoteDevice — 后者由 listener
//! 任务在 handshake 返回后组装。

use crate::Error;
use futures::io::AsyncRead;
use futures::io::AsyncWrite;
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

/// MSI-X 表 + PBA 占用的 BAR index（spec §3.6）。
///
/// v2 规范：emulated device 用专用 BAR4 暴露 MSI-X table/PBA（参 nvme/pci.rs）。
/// host 在 DeviceDescribe.bars 中**必须不**声明 `index == MSIX_BAR_INDEX`
/// 的 BAR；冲突 → handshake 拒绝。
///
/// 这避免了"host 的 BAR 与 MSI-X table 在同一 window 重叠"的边界处理，
/// 让 MmioIntercept 路由是简单的 `if bar_idx == MSIX_BAR_INDEX { ... }`。
pub const MSIX_BAR_INDEX: u8 = 4;

/// 在 connected transport 上完成应用层握手。
///
/// 调用方负责在外层加超时（如 `CancelContext::with_timeout`）。
/// 返回 `(describe, transport)`：transport 被消费再返回，便于 `PolledSocket`
/// 这种内部含状态的类型继续被后续 reader/writer 使用。
pub async fn handshake<T>(
    mut transport: T,
    instance_id: guid::Guid,
) -> Result<(DeviceDescribe, T), Error>
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
    Ok((describe, transport))
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
        // v2 K-NEW-B: host 不得占用 MSI-X 专用 BAR。
        if bar.index as u8 == MSIX_BAR_INDEX {
            return Err(Error::BarLayout(format!(
                "BAR index {} reserved for MSI-X table/PBA; host must use other indices",
                MSIX_BAR_INDEX
            )));
        }
        // v1 限制：DeviceBars upstream API 仅暴露 bar0/bar2/bar4。
        // BAR1/3/5 暂不支持（resolver 装配时只能 silently 丢弃），所以
        // 在 handshake 阶段就拒绝，让 host 配置错误立刻可见而不是 boot 后
        // 静默"丢 BAR"。pci_core 升级 API 暴露 bar1/3/5 后可放开。
        if matches!(bar.index, 1 | 3 | 5) {
            return Err(Error::BarLayout(format!(
                "BAR index {} not yet supported by v1 (DeviceBars upstream only \
                 exposes bar0/bar2; use 0 or 2)",
                bar.index
            )));
        }
        if bar.index > 5 {
            return Err(Error::BarLayout(format!(
                "BAR index {} > 5 (PCI type 0 only has BAR0..BAR5)",
                bar.index
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
pub(crate) mod tests_helpers_assert_validate_ok {
    /// 让 resolver.rs 的测试能调本模块的 validate_describe（避免 pub
    /// 化只为测试用）。
    pub(crate) fn _check(d: &pcie_remote_protocol::DeviceDescribe) {
        super::validate_describe(d).expect("describe must validate");
    }
}

#[cfg(test)]
pub(crate) fn tests_helpers_assert_validate_ok(d: &pcie_remote_protocol::DeviceDescribe) {
    tests_helpers_assert_validate_ok::_check(d);
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

    /// v2 K-NEW-B: host 不能声明 MSIX_BAR_INDEX (BAR4)。
    #[test]
    fn rejects_msix_bar_collision() {
        let mut d = good_describe();
        d.bars[0].index = MSIX_BAR_INDEX as u32;
        let r = validate_describe(&d);
        assert!(r.is_err());
        let msg = format!("{:?}", r.err().unwrap());
        assert!(msg.contains("reserved for MSI-X"), "msg = {msg}");
    }

    /// PCI type 0 仅 BAR0..BAR5，拒绝 index 6+。
    #[test]
    fn rejects_oversized_bar_index() {
        let mut d = good_describe();
        d.bars[0].index = 6;
        assert!(validate_describe(&d).is_err());
    }

    /// v2 限制：BAR1/3/5 在 v1 不支持（DeviceBars upstream API 限制）。
    #[test]
    fn rejects_unsupported_bar_index() {
        for idx in [1u32, 3, 5] {
            let mut d = good_describe();
            d.bars[0].index = idx;
            let r = validate_describe(&d);
            assert!(r.is_err(), "BAR index {idx} should be rejected in v1");
            let msg = format!("{:?}", r.err().unwrap());
            assert!(msg.contains("not yet supported"), "msg = {msg}");
        }
    }
}
