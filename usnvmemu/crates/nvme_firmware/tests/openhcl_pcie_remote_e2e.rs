// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # OpenHCL pcie_remote transport — 跨进程 e2e harness
//!
//! 验证 `nvme_firmware` 经 **pcie_remote 协议**（OpenHCL/OpenVMM 第 1 条接入）被正确
//! 描述与驱动。本 harness 是 nvme-of `scripts/interop_py/` 与 vfio `scripts/qemu_interop/`
//! 在 pcie_remote transport 上的**对位独立 oracle**——此前该 transport 只有 noop 设备
//! 烟雾测试（`pcie_remote_test_harness`），从未驱动过真 NVMe firmware。
//!
//! ## 角色
//!
//! pcie_remote 协议里 **device 侧** 收 `Hello` / 回 `HelloAck` / 收 `ToHost`(MMIO、
//! DMA 回执) / 发 `ToOpenhcl`(DMA 请求、中断)；**OpenHCL/VTL2 侧** 反之。
//!
//! - 被测：真 `nvme_firmware` bin，`--tcp-addr`（= pcie_remote **device 侧**，TCP client）。
//! - 本 harness：**OpenHCL 侧**（TCP server）——发 Hello、收 HelloAck、（O2+）发 MMIO、
//!   按自己的 "guest memory" 缓冲服务 firmware 的 DMA 请求。
//!
//! ## 增量
//!
//! - **O1（本文件当前）**：握手 + NVMe 身份描述。
//! - O2：admin queue（CC.EN → CSTS.RDY，doorbell→SQE-DMA→Identify→CQE-DMA→IRQ）。
//! - O3：4K Format + IO round-trip + fused C&W（以 guest-mem 作独立 oracle）。
//!
//! ## 跑法
//!
//! ```bash
//! cargo test --test openhcl_pcie_remote_e2e        # 默认 features 含 openhcl
//! ```
//!
//! 仅 Unix：依赖 bin 的 `--tcp-addr` 路径（vsock `--vm-id` 是 Windows-only，留 L3 真
//! Hyper-V guest e2e）。

#![cfg(unix)]

use anyhow::Context;
use anyhow::Result;
use pcie_remote_protocol::Hello;
use pcie_remote_protocol::HelloAck;
use pcie_remote_protocol::PROTOCOL_MAGIC;
use pcie_remote_protocol::PROTOCOL_VERSION;
use pcie_remote_protocol::codec;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

/// NVMe class code：Mass Storage(0x01) / NVM(0x08) / NVMe programming IF(0x02)。
const NVME_CLASS_CODE: u32 = 0x01_0802;
/// bin `--vid` 默认 0x1414 = Microsoft（配合 OpenHCL 默认路由）。
const DEFAULT_VID: u32 = 0x1414;

/// 子进程 + 临时文件守卫：Drop 时 kill child + 删 backing/log；测试 panic 时打印
/// firmware 日志尾部（否则跨进程失败盲调）。**先于 spawn 构造**（child=None），
/// 这样即便 `Command::spawn` 失败，临时文件也被清掉（reviewer LOW-1）。
struct Harness {
    child: Option<std::process::Child>,
    backing: PathBuf,
    /// firmware stdout+stderr 落盘处（tracing 默认走 stdout）。
    log: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // 仅在测试 panic（断言失败 / 超时）时把 firmware 日志尾部吐出来，便于诊断。
        if std::thread::panicking()
            && let Ok(s) = std::fs::read_to_string(&self.log)
        {
            let lines: Vec<&str> = s.lines().collect();
            let tail = lines[lines.len().saturating_sub(40)..].join("\n");
            if !tail.trim().is_empty() {
                eprintln!("\n--- nvme_firmware 日志尾部 (诊断用) ---\n{tail}\n--- end ---\n");
            }
        }
        let _ = std::fs::remove_file(&self.backing);
        let _ = std::fs::remove_file(&self.log);
    }
}

/// 起 TCP listener（127.0.0.1:0 取空闲端口）→ 起真 `nvme_firmware --tcp-addr`（它作为
/// client 连过来，自带 connect 重试）→ accept。返回**裸 `TcpStream`**（调用方按需
/// `.compat()` 或 `into_split()`——O2 的并发 pump 要 split），以及守卫。
async fn spawn_and_accept() -> Result<(TcpStream, Harness)> {
    // listener 先就绪，bin 的 connect_with_retry 立即连上（无 accept-before-connect race）。
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind 127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    // 唯一名临时文件（pid+port）。
    let mut backing = std::env::temp_dir();
    backing.push(format!(
        "openhcl_pcie_remote_e2e_ns_{}_{}.img",
        std::process::id(),
        port
    ));
    let mut log = std::env::temp_dir();
    log.push(format!(
        "openhcl_pcie_remote_e2e_log_{}_{}.txt",
        std::process::id(),
        port
    ));

    // 4 MiB backing → open() 有合法 NS 容量（÷512 = 8192 LBA）。
    let f = std::fs::File::create(&backing).context("create backing file")?;
    f.set_len(4 << 20).context("set_len backing")?;
    drop(f);

    // **先构造守卫**（child=None），保证后续任何 `?` 早退都清理临时文件。
    let mut harness = Harness {
        child: None,
        backing: backing.clone(),
        log: log.clone(),
    };

    // 捕获 firmware stdout+stderr 到 log 文件（tracing 走 stdout；anyhow/panic 走 stderr）。
    let log_file = std::fs::File::create(&log).context("create log file")?;
    let log_file2 = log_file.try_clone().context("clone log fd")?;

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_nvme_firmware"))
        .arg("--tcp-addr")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--backing-file")
        .arg(&backing)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file2))
        .spawn()
        .context("spawn nvme_firmware bin（确认 default features 含 openhcl）")?;
    harness.child = Some(child);

    // bin 连过来（connect_with_retry 兜底慢启动）。
    let (stream, _peer) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
        .await
        .context("accept 超时 —— nvme_firmware bin 未在 15s 内连上 TCP")?
        .context("accept")?;
    Ok((stream, harness))
}

/// O1 —— pcie_remote 握手 + NVMe 身份。
///
/// 该 transport 上**第一个**驱动真 NVMe firmware 的跨进程测试：证明 firmware 经真
/// pcie_remote wire 把自己描述成**一个 NVMe 设备**（vendor/class/BAR/MSI-X）。
///
/// 诚实边界：旧 noop harness 也广告同样的 vendor/class/BAR（见
/// `pcie_remote_test_harness/src/main.rs`），故 O1 只证 "经真 wire 描述成 NVMe"，**尚未**
/// 钉死"就是这份 firmware controller"——后者要等 O2 真 enable controller + 读回 CAP/CSTS
/// + 拉到 Identify Controller payload（MN/SN/FR）才赚到。
#[tokio::test]
async fn openhcl_handshake_and_nvme_identity() -> Result<()> {
    let (stream, _harness) = spawn_and_accept().await?;
    let mut wire = stream.compat();

    // OpenHCL 侧主动发 Hello。
    let hello = Hello {
        magic: PROTOCOL_MAGIC,
        version: PROTOCOL_VERSION,
        instance_id: Vec::new(),
    };
    codec::write_frame(&mut wire, &hello)
        .await
        .context("write Hello")?;

    // 收 firmware 的 HelloAck + DeviceDescribe（独立 oracle：经真 wire 过来）。
    let ack: HelloAck = tokio::time::timeout(Duration::from_secs(5), codec::read_frame(&mut wire))
        .await
        .context("read HelloAck 超时")?
        .context("read HelloAck")?;
    assert!(ack.ok, "HelloAck.ok=false: {}", ack.reason);
    let dev = ack.device.context("HelloAck 缺 DeviceDescribe")?;

    // 断言 NVMe 身份。
    assert_eq!(
        dev.vendor_id, DEFAULT_VID,
        "vendor_id 应 0x1414 (Microsoft 默认)"
    );
    assert_eq!(
        dev.class_code, NVME_CLASS_CODE,
        "class_code 应 0x010802 (NVMe)，实为 {:#08x}",
        dev.class_code
    );
    assert!(!dev.bars.is_empty(), "NVMe 须至少 BAR0");
    let bar0 = &dev.bars[0];
    assert_eq!(bar0.index, 0, "首 BAR 应是 index 0");
    assert!(
        bar0.size >= 0x1000,
        "BAR0 应 ≥ 4 KiB(NVMe 寄存器组)，实为 {:#x}",
        bar0.size
    );
    assert!(dev.msix_count >= 1, "NVMe 须 ≥ 1 个 MSI-X vector");

    Ok(())
}
