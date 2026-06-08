# pcie_vfio_user_sdk

vfio-user (Nutanix `libvfio-user` 协议) Transport backend for
`pcie_remote_userspace_sdk` `PcieDevice` 实现，让同一份用户态 PCIe 设备通过
`qemu -device vfio-user-pci,socket=/tmp/dev.sock` 直接被 QEMU 接管。

> ✅ SHIPPED — Phase U1 / U2 / U3 / U4 / U5 + U-followup (commits
> `4f0549e8` / `1034c5b5` / `7e5eb9b5` / U4 / `de871b2f` / `2d284030`)。

## 用途

把 [`pcie_remote_nvme_userspace`](../pcie_remote_nvme_userspace/) 或任何
基于 [`pcie_remote_userspace_sdk`](../pcie_remote_userspace_sdk/) 的设备
跑成 vfio-user server (UNIX socket)，QEMU / Cloud Hypervisor / SPDK
可作为 client 接管，guest 看到普通 PCIe 设备 (inbox driver 就消费)。

## 模块

- `proto.rs` — vfio-user wire (16 B common header + message body)
- `framing.rs` — UNIX socket framing (header + body 分两轮 read)
- `handshake.rs` — VERSION negotiate (U2)
- `session.rs` + `server.rs` — accept loop + per-conn session
- `dma.rs` — DMA region 注册 (U3 REGION_INFO + U5 SET_IRQS eventfd)
- `irq.rs` — INTx / MSI / MSI-X 桥 (U5)
- `transport.rs` — `impl Transport for VfioUserSession` (U5)
- `lib.rs` — `VfioUserListener::bind(path)`

## 谁在用

| Consumer | 用法 |
|----------|------|
| **`pcie_remote_nvme_userspace` bin** (`--vfio-user-socket`) | QEMU 接管模式 |

具体 e2e 命令：见 `pcie_remote_nvme_userspace/QEMU_VFIO_USER.md`。

## spec

- wire reference: [`../../specs/2026-06-04-vfio-user-wire-reference.md`](../../specs/2026-06-04-vfio-user-wire-reference.md)
- client design (OpenVMM 一侧, 用作 client 时): [`../../specs/2026-06-05-vfio-user-client-design.md`](../../specs/2026-06-05-vfio-user-client-design.md)
- ROADMAP: [`../../plans/ROADMAP.md`](../../plans/ROADMAP.md)
