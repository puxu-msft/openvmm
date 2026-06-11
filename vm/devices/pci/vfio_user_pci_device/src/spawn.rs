// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot-time connect 任务（W6b Task 1.5）。
//!
//! 为每个 instance 启动一个 connect 任务：
//!   1. 主动 `connect` 到 firmware 的 vfio-user AF_UNIX socket（client，**非**
//!      模板的 listen/accept server 模型）
//!   2. 连接 / handshake / identity 任意一步出错 → 100ms backoff 后重试
//!   3. 总超时由 [`CancelContext::with_timeout`] 控制；重试上限
//!      [`MAX_CONNECT_ATTEMPTS`] 防恶意 host 用大 timeout 拉到无限重试
//!   4. 成功 → 把 [`PreparedVfioUserDevice`] 插入 prepared map，**return**
//!      （W6b connect 一次即可；reconnect 是 W6d，**不**listen-forever）
//!   5. 超时 / 重试耗尽 → 打 warning 后 return（resolver 在 prepared 缺项时
//!      发放 `AbsentPcieDevice`，boot DoS 兜底）；**绝不** panic
//!
//! connect 任务返回 [`Task<()>`]；任务在成功 / 失败后自然结束，prepared 在 map
//! 中等 resolver 取走（与模板 listener task 的 prepared_map 路径同形）。

use crate::prepared::PreparedMap;
use crate::prepared::PreparedVfioUserDevice;
use cvm_tracing::CVM_ALLOWED;
use mesh::CancelContext;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::time::Duration;
use vfio_user_device::VfioUserClient;
use vfio_user_wire::proto::pci_irq;
use vfio_user_wire::proto::pci_region;

/// Per-instance connect 尝试上限。拒绝恶意 host 用大 timeout 把 vm boot 期间拉到
/// 无限重试。32 与模板 `MAX_ACCEPT_ATTEMPTS` 一致（仓库 ttrpc / pipette 常见值）。
const MAX_CONNECT_ATTEMPTS: u32 = 32;

/// connect 失败 / handshake 失败后的 backoff，与模板一致。
const BACKOFF: Duration = Duration::from_millis(100);

/// 从 PCI config header 三个 dword 解析 [`HardwareIds`]。
///
/// **M-2：严格按 PCI 寄存器位布局解析，不抄任何 protobuf `class_code` 的移位逻辑。**
/// NVMe 的 class 三元组 = base_class 0x01（Mass Storage Controller）/ sub_class 0x08
/// （Non-Volatile Memory Controller）/ prog_if 0x02（NVM Express）。
///
/// - `id_dword`：config offset 0 的 4 个 LE 字节。
///   `vendor_id = [b0,b1]`，`device_id = [b2,b3]`。
/// - `class_dword`：config offset 0x08 的 CLASS_REVISION 寄存器（u32, LE 4 字节）。
///   bits 7-0 = `revision_id`；bits 15-8 = `prog_if`；bits 23-16 = `sub_class`；
///   bits 31-24 = `base_class`。
/// - `sub_dword`：config offset 0x2c 的 4 个 LE 字节。
///   `type0_sub_vendor_id = [b0,b1]`，`type0_sub_system_id = [b2,b3]`。
pub fn derive_hardware_ids_from_cfg(
    id_dword: &[u8],
    class_dword: &[u8],
    sub_dword: &[u8],
) -> anyhow::Result<HardwareIds> {
    if id_dword.len() < 4 {
        anyhow::bail!("id_dword 过短：{} < 4", id_dword.len());
    }
    if class_dword.len() < 4 {
        anyhow::bail!("class_dword 过短：{} < 4", class_dword.len());
    }
    if sub_dword.len() < 4 {
        anyhow::bail!("sub_dword 过短：{} < 4", sub_dword.len());
    }

    let vendor_id = u16::from_le_bytes([id_dword[0], id_dword[1]]);
    let device_id = u16::from_le_bytes([id_dword[2], id_dword[3]]);

    // CLASS_REVISION 寄存器：单 u32，按位段拆。
    let d = u32::from_le_bytes([
        class_dword[0],
        class_dword[1],
        class_dword[2],
        class_dword[3],
    ]);
    let revision_id = d as u8;
    let prog_if = (d >> 8) as u8;
    let sub_class = (d >> 16) as u8;
    let base_class = (d >> 24) as u8;

    let type0_sub_vendor_id = u16::from_le_bytes([sub_dword[0], sub_dword[1]]);
    let type0_sub_system_id = u16::from_le_bytes([sub_dword[2], sub_dword[3]]);

    Ok(HardwareIds {
        vendor_id,
        device_id,
        revision_id,
        prog_if: ProgrammingInterface::from(prog_if),
        sub_class: Subclass::from(sub_class),
        base_class: ClassCode::from(base_class),
        type0_sub_vendor_id,
        type0_sub_system_id,
    })
}

/// 在一个**已 connect** 的 client 上做 handshake + 读身份/几何，构造
/// [`PreparedVfioUserDevice`]。
///
/// 抽出此函数是为了单测可达（loopback 用 `VfioUserClient::from_stream` 喂一条
/// 已建立的 `UnixStream`，绕过真实 listening unix path）；
/// [`spawn_vfio_user_connects`] 的 connect-by-path 只是它外面的薄重试层。
///
/// packed RegionInfoPayload / IrqInfoPayload / DeviceInfoPayload 字段先 copy 到本地
/// （`#[repr(C,packed)]` 直接取字段是 E0793）。
pub async fn prepare_from_client(
    mut client: VfioUserClient,
) -> anyhow::Result<PreparedVfioUserDevice> {
    client.handshake().await?;

    // 健全性：读 region/irq 总数（仅日志，不做强校验——PCI 标准常量由 server 决定）。
    let info = client.get_device_info().await?;
    let num_regions = info.num_regions; // packed copy
    let num_irqs = info.num_irqs; // packed copy
    tracing::info!(
        CVM_ALLOWED,
        num_regions,
        num_irqs,
        "vfio_user_pci: device info"
    );

    // BAR0 几何。
    let bar0 = client.get_region_info(pci_region::BAR0).await?;
    let bar0_size = {
        let s = bar0.size; // packed copy
        s
    };

    // MSI-X 向量数。
    let irq = client.get_irq_info(pci_irq::MSIX).await?;
    let msix_count = {
        let c = irq.count; // packed copy
        c as u16
    };

    // 身份：CONFIG region 三个 dword。
    let id = client.region_read(pci_region::CONFIG, 0, 4).await?;
    let class = client.region_read(pci_region::CONFIG, 0x08, 4).await?;
    let sub = client.region_read(pci_region::CONFIG, 0x2c, 4).await?;
    let hardware_ids = derive_hardware_ids_from_cfg(&id, &class, &sub)?;

    Ok(PreparedVfioUserDevice {
        client,
        hardware_ids,
        bar0_size,
        msix_count,
    })
}

/// 为一组 unix-path-based instance 启动 connect 任务，结果填入 `prepared`。
///
/// 返回 connect tasks（调用方持有 Vec 不被 drop）。每个任务把「bounded-retry connect
/// 循环」包进 `CancelContext::new().with_timeout(timeout).until_cancelled(...)`：
/// - connect 失败 → backoff + 重试
/// - connect 成功 → `prepare_from_client`；成功则插入 prepared map + **return**
///   （connect 一次，非 listen-forever）；prepare 失败 → backoff + 重试
/// - 重试耗尽 / 超时 → warning + return（resolver 发 `AbsentPcieDevice` 兜底）
pub fn spawn_vfio_user_connects(
    driver: impl Driver + Clone + 'static,
    spawner: impl Spawn + Clone + 'static,
    instances: Vec<(guid::Guid, String, Duration)>,
    prepared: PreparedMap,
) -> Vec<Task<()>> {
    let mut tasks = Vec::new();
    for (id, unix_path, timeout) in instances {
        let driver = driver.clone();
        let prepared = prepared.clone();
        let task = spawner.spawn(format!("vfio_user_connect_{id}"), async move {
            let mut ctx = CancelContext::new().with_timeout(timeout);
            let driver_for_backoff = driver.clone();
            let outcome = ctx
                .until_cancelled(async {
                    let mut attempts: u32 = 0;
                    loop {
                        if attempts >= MAX_CONNECT_ATTEMPTS {
                            tracing::error!(
                                CVM_ALLOWED,
                                %id,
                                attempts,
                                "vfio_user_pci: per-instance connect attempt cap reached"
                            );
                            return;
                        }
                        attempts += 1;
                        let client = match VfioUserClient::connect(&driver, &unix_path).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!(
                                    CVM_ALLOWED,
                                    %id,
                                    error = %e,
                                    "vfio_user_pci: connect failed; backing off"
                                );
                                PolledTimer::new(&driver_for_backoff).sleep(BACKOFF).await;
                                continue;
                            }
                        };
                        match prepare_from_client(client).await {
                            Ok(prep) => {
                                let vendor_id = prep.hardware_ids.vendor_id;
                                let device_id = prep.hardware_ids.device_id;
                                prepared.lock().insert(id, prep);
                                tracing::info!(
                                    CVM_ALLOWED,
                                    %id,
                                    vendor_id,
                                    device_id,
                                    "vfio_user_pci: connect+handshake ok, prepared inserted (boot grace)"
                                );
                                // W6b：connect 一次即停（reconnect 是 W6d）。
                                return;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    CVM_ALLOWED,
                                    %id,
                                    error = %e,
                                    "vfio_user_pci: handshake/identity failed; backing off"
                                );
                                PolledTimer::new(&driver_for_backoff).sleep(BACKOFF).await;
                                continue;
                            }
                        }
                    }
                })
                .await;
            if outcome.is_err() {
                tracing::warn!(
                    CVM_ALLOWED,
                    %id,
                    "vfio_user_pci: connect timeout; resolver 将发放 AbsentPcieDevice 兜底"
                );
            }
        });
        tasks.push(task);
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// derive：NVMe 三元组 + vendor/device 解析。
    #[test]
    fn derive_hardware_ids_nvme_triple() {
        // vendor 0x1234 / device 0x5678（LE）。
        let id_dword = [0x34, 0x12, 0x78, 0x56];
        // base 0x01 / sub 0x08 / prog_if 0x02 / rev 0x01（LE: rev,prog_if,sub,base）。
        let class_dword = [0x01, 0x02, 0x08, 0x01];
        let sub_dword = [0, 0, 0, 0];

        let ids =
            derive_hardware_ids_from_cfg(&id_dword, &class_dword, &sub_dword).expect("derive ok");
        assert_eq!(ids.vendor_id, 0x1234);
        assert_eq!(ids.device_id, 0x5678);
        assert_eq!(ids.revision_id, 0x01);
        assert_eq!(ids.prog_if, ProgrammingInterface::from(0x02u8));
        assert_eq!(ids.sub_class, Subclass::from(0x08u8));
        assert_eq!(ids.base_class, ClassCode::from(0x01u8));
        assert_eq!(ids.type0_sub_vendor_id, 0);
        assert_eq!(ids.type0_sub_system_id, 0);
    }

    /// derive：任一 slice < 4 字节 → Err。
    #[test]
    fn derive_hardware_ids_short_slice_errors() {
        assert!(derive_hardware_ids_from_cfg(&[0, 1, 2], &[0, 0, 0, 0], &[0, 0, 0, 0]).is_err());
        assert!(derive_hardware_ids_from_cfg(&[0, 0, 0, 0], &[0, 1], &[0, 0, 0, 0]).is_err());
        assert!(derive_hardware_ids_from_cfg(&[0, 0, 0, 0], &[0, 0, 0, 0], &[0]).is_err());
    }
}
