# 真 QEMU vfio-user e2e harness

用**真 QEMU**（独立 C 实现的 vfio-user 客户端）驱动本 crate 的 vfio-user
**服务端**，做差分 oracle 验证——而非自己写 client 验自己写的 server。

## 为什么需要它

自己写 server + 自己写测试 client 会共享同一套错误假设，两边都不报错
（见 [/usnvmemu/docs/LESSONS.md](/usnvmemu/docs/LESSONS.md) §20：self-consistent
≠ spec-conformant）。`scripts/interop_py/` 的 Python harness 已能跨进程验
config-space 枚举，但它的 wire 编解码仍是我们自己写的。真 QEMU 是**第三方
独立实现**，曾用它抓出两个自家测试全绿却真实存在的握手 bug：

- **version minor 协商**：原 reply 硬编码 `minor=1`，QEMU 11 提议 `minor=0`，
  回 1 > 0 被判 `incompatible server version`。spec 要求 `min(client, server)`。
- **max_msg_fds**：原广告 128 > QEMU `VFIO_USER_MAX_MAX_FDS=16` → `malformed
  max_msg_fds`。改广告 8（QEMU 默认 `DEF_MAX_FDS`）。

两个修复 + 回归测试见 commit `f8fdf220`（`src/handshake.rs`）。

## QEMU 版本要求

vfio-user 客户端（`vfio-user-pci` 设备）由 Nutanix/John Levon 在 **QEMU 10.1**
（2025-08）合入上游。QEMU < 10.1（如发行版常见的 8.2）**不带** vfio-user
客户端，无法用本 harness。本仓库用 **linuxbrew QEMU 11.0.1** 验证：

```bash
eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
qemu-system-x86_64 --version   # 须 ≥ 10.1
```

## 跑

```bash
# 1) 编译带 vfio-user feature 的 server（usnvmemu 各 crate 独立、非 workspace，
#    须 cd 进 crate 编译；不能用 cargo -p）
cd usnvmemu/crates/nvme_firmware
cargo build --features vfio-user

# 2) 让 QEMU 11 进 PATH
eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"

# 3) 跑 harness（在本目录）
cd ../vfio_user_transport/scripts/qemu_interop
python3 run_qemu_vfio.py
```

期望输出（PASS）：

```
=== server socket up；启动真 QEMU vfio-user-pci 接管 ===
=== QEMU query-pci 看到的设备 ===
  vendor=0x8086 device=0x29c0 class=Host bridge
  ...
  vendor=0x1414 device=0xc0de class=?  <<< 我们的 NVMe vfio-user 设备!
=== PASS: 真 QEMU vfio-user-pci realize 成功 — guest 侧 PCI 总线看到 NVMe(...) ===
```

## 做了什么

1. spawn `nvme_firmware --vfio-user-sock <sock> --backing-file <img>`；
2. 起 QEMU `q35 + memfd 共享内存（DMA_MAP 需 share=on）+ -device
   {"driver":"vfio-user-pci","socket":{...}} + QMP + -S`（暂停 CPU，只验
   realize/握手，不引导 guest OS）；
3. device realize 走完整 `VERSION → GET_INFO → GET_REGION_INFO ×9 → config
   空间读 → DMA_MAP` 握手——任一环节 wire 不合规，QEMU realize 直接失败；
4. QMP `query-pci` 确认 guest 侧 PCI 总线真看到 vendor `0x1414` / device
   `0xc0de`（我们的 NVMe 控制器）。

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `QEMU_BIN` | 自动探测 linuxbrew / `PATH` | QEMU system 二进制 |
| `NVME_BIN` | crate-local `target/debug/nvme_firmware` | server 二进制 |
| `KEEP_LOGS` | 空（清理） | 非空则保留临时目录日志 |

## 后续（未实现）

- 引导真 guest kernel + rootfs，看 `/dev/nvme0` 块设备真读写（需 IO queue
  DMA 全路径 + initramfs）。当前 `-S` 只验到 realize/PCI 枚举。
- 把本 harness 接进 CI（需 CI runner 有 ≥10.1 QEMU + KVM/TCG）。
