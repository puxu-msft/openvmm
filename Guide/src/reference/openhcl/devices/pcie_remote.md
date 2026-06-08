# pcie_remote 实验设备

> ⚠ **仅用于实验/调试**。不在 CVM 上启用（spec §3.10 硬禁）。

## 概述

`pcie_remote` 让 guest 看到一个 PCIe 设备，但所有设备语义由 Windows host 上的用户态实验程序实现。OpenHCL/OpenVMM 内只保留极薄外壳。常用于：

- 快速原型 NVMe 控制器或其他 PCIe 设备的实现
- 驱动 fuzz / error injection 测试
- 在 host 端用任意语言（不限 Rust）实现设备

## 4 种部署形态

| 形态 | host 是谁 | 通道 | CLI |
|------|---------|------|-----|
| A. OpenVMM 启 OpenHCL（开发） | OpenVMM 进程 | TCP loopback | OpenVMM `--pcie-remote rc0rp0,socket=127.0.0.1:48914` |
| B. OpenHCL + 自建 IGVM | Hyper-V vmwp | vsock | OpenHCL cmdline `--pcie-remote-instance <guid>:<port>` |
| C. OpenHCL + 生产 Hyper-V（占位 NVMe） | Hyper-V vmwp | vsock | `Add-VMNvmeController` + `--pcie-remote-takeover <nvme_guid>:<port>` |
| D. OpenVMM-only | OpenVMM 进程 | TCP loopback | 同 A |

## 路径 A / D（OpenVMM，最易跑通）

1. 编译 OpenVMM：`cargo build -p openvmm`
2. 编译并启动 host stub：
   ```bash
   cd usnvmemu/crates/pcie_remote_noop_host
   cargo run
   ```
   预期看到 `pcie_remote_noop_host: listening (loopback only)`。
3. 启动 OpenVMM 加 CLI 参数：
   ```bash
   target/debug/openvmm \
     --processors 2 --memory 1G \
     --pcie-remote rc0rp0,socket=127.0.0.1:48914 \
     <其余 kernel / initrd 参数>
   ```
4. guest dmesg 应显示一个 vendor=1414 device=c0de 的 PCIe 设备。

### WSL2 注意

WSL2 默认开启 `localhostForwarding`，OpenVMM 在 WSL 内 bind `127.0.0.1` 会被自动转发到 Windows host **任意用户进程**。生产 / 多用户 Windows host 必须改用形态 B 或 C（vsock + ACL），不要用 TCP loopback。

## 路径 B（OpenHCL + 自建 IGVM）

1. fork 仓库并自建 IGVM，cmdline policy 选 `APPEND_CHOSEN`。
2. host 上一次性注册 service GUID + ACL（管理员 PowerShell）：
   ```powershell
   .\docs\superpowers\scripts\setup-pcie-remote.ps1 -VsockPort 50000
   ```
3. host 实验程序用 AF_HYPERV connect 到 `(target_vm_id, computed_service_guid)`。
4. OpenHCL boot cmdline append：
   ```
   --pcie-remote-instance 28ed784d-c059-429f-9d9a-46bea02562c0:50000,handshake_timeout_ms=5000
   ```

## 路径 C（生产 Hyper-V，占位 NVMe 接管）

vmwp 不知道也不会理解任何非标准 GUID，所以借用一个已注册的 NVMe controller GUID：

1. 用 PowerShell 创建一个 NVMe controller（不绑后端磁盘）：
   ```powershell
   $Vm = "MyVM"
   Add-VMNvmeController -VMName $Vm
   $Ctrl = (Get-VMNvmeController -VMName $Vm)[0]
   $NvmeGuid = $Ctrl.Id  # 这就是 vmwp 已知的 GUID
   ```
2. 注册 service GUID + ACL（同形态 B）。
3. OpenHCL cmdline append：
   ```
   --pcie-remote-takeover <NvmeGuid>:50000
   ```
4. OpenHCL 在 vtl2_settings_worker 内拦截这个 GUID 不走 NVMe controller，而改派到 pcie_remote 路径。

## 安全要点

- **不提供 IOMMU 等价隔离**：host 实验程序的可信级别必须 ≥ Hyper-V VSP（spec §3.7）。
- **CVM 硬禁**：isolation=SNP/TDX/VBS 时 OpenHCL 静默过滤所有 pcie_remote 配置。
- vsock ACL 默认仅 Admin/SYSTEM；任何能 connect 的进程都拥有读写 guest VTL0 RAM + 注入任意 MSI 的能力。
- v1 不支持热插拔 / save_restore；servicing 期间设备会消失，guest 应提前手动卸载。

## 进入 Lost 状态后的行为

host 断开 / 协议错 / 帧过大 / dead-man trip → 设备进 Lost：
- cfg read 返 Err(InvalidRegister) → vpci 路径下 guest 读到 0xFFFFFFFF
- cfg write 静默丢弃
- MMIO 读写返 NoResponse
- in-flight DeferredRead/Write 全部 complete_error(NoResponse)

v1 不支持重连。需要 host 重启后整 VM 重启。

## 参考

- [设计文档](../../../../usnvmemu/docs/specs/2026-05-29-pcie-remote-design.md)
- [实施计划](../../../../usnvmemu/docs/plans/2026-05-29-pcie-remote-impl.md)
- [setup.ps1](../../../../docs/superpowers/scripts/setup-pcie-remote.ps1)
- [host SDK 示例](../../../../usnvmemu/crates/pcie_remote_noop_host/)
