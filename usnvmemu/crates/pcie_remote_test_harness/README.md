# pcie_remote_test_harness

**最小 host stub** — 用于 PCIe Remote 协议 e2e 测试 + stress harness。

> ✅ SHIPPED — 一系列 commit 含 `c6f28f75` periodic ReadGpa / `2f8ffe75`
> periodic WriteGpa / `e79d21a7` class_code serial / `265dfb73` `--stress-dma-count` /
> `a6c36b5b` `--stress-bad-frames` 等。

## 用途

OpenHCL VTL2 / OpenVMM 端是 **server**，本 stub 是 **client**：
- 主动 connect server 端口 (`127.0.0.1:48914` TCP, 或 vsock service GUID)
- 回 HelloAck 描述一个 noop 设备
- 任何 MMIO read → 0；cfg side-effect / write → 静默丢弃
- 可加 `--stress-dma-count` / `--stress-bad-frames` 等 flag 产 e2e 流量

## 两个 binary

- `pcie_remote_test_harness_tcp` (`src/main.rs`) — TCP 路径 (OpenVMM dev path)
- `pcie_remote_test_harness_vsock` (`src/vsock_main.rs`) — vsock 路径 (OpenHCL VTL2)

## 真用法

主要给测试用：跑 OpenHCL VTL2 + worker spawn 流程验证；详
[../../PCIE_REMOTE_SESSION_LOG.md](../../PCIE_REMOTE_SESSION_LOG.md) 早期 "noop_host" 段 + spec
[../../specs/2026-05-29-pcie-remote-design.md](../../specs/2026-05-29-pcie-remote-design.md)
K-NEW-A..E + K-NEW-G/H 段。

## 升级路径

要写真有 device 行为的 host 端，**不要扩 noop_host**；用
[`pcie_device_sdk`](../pcie_device_sdk/) +
`nvme_firmware` / `rng_device_example` 作为模板。
noop_host 是协议 e2e harness，不是 device 实现模板。
