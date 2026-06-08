# pcie_device_sdk

Userspace SDK for implementing PCIe devices over the **pcie_remote vsock
(OpenHCL) / TCP (OpenVMM)** protocol。

> ✅ SHIPPED (Phase N1+N1b, commits in 2026-05-30..2026-05-31)。

## 用途

让用户写新 PCIe device 时不必碰 vsock/protobuf 框架，只实现
`trait Transport` 5 个原语 (`dma_read` / `dma_write` /
`dma_*_fire_and_forget` / `fire_interrupt`) — backend (vsock / TCP /
vfio-user) 自动注入。

`DeviceCtx` 是核心 type：
- 持有 transport
- 跑 in/out queue
- spawn 持续 worker loop
- `for_testing()` (Phase Q10) 给下游 mock 注入

## 谁在用

| Consumer | 路径 |
|----------|------|
| **`nvme_firmware`** | 整个 NVMe controller 教学库 |
| **`rng_device_example`** | 第二个 PcieDevice 教学 example (最小 RNG) |
| **`nvme_of_tcp_target`** | NVMe-oF TCP target (在 SDK 之上加 NVMe-oF 包装) |
| **vfio-user backend (`vfio_user_transport`)** | 让同一份 device 接 QEMU |

## 设计原则

- `trait Transport` 5 原语；不暴露 protobuf 给 device 作者
- `Send + Sync` boundaries minimal — device 作者写 sync 业务，SDK 桥
  async I/O
- 不假设 backing；NVMe / RNG / 任何 device 都行
- 单测 friendly: `for_testing()` 注入 buffer

## 参考

- 上层教学：[`../nvme_firmware/README.md`](../nvme_firmware/README.md)
- spec：[`../../specs/2026-05-29-pcie-remote-design.md`](../../specs/2026-05-29-pcie-remote-design.md)
- vsock / TCP framing：[`../../specs/2026-06-04-vfio-user-wire-reference.md`](../../specs/2026-06-04-vfio-user-wire-reference.md) (vfio-user 一脉)
