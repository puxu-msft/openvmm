# pcie_remote_rng_userspace

最小 PCIe 硬件 RNG (random number generator) 教学 example — **第二个**基于
[`pcie_remote_userspace_sdk`](../pcie_remote_userspace_sdk/) 的 PcieDevice
实现，证明 SDK 不止能写 NVMe。

> ✅ SHIPPED — Phase I3 (commit `7402dd62`)。

## 硬件模型

- BAR0 (MMIO32) 6 个 register: STATUS / CTRL / SEED_LO / SEED_HI /
  DMA_GPA / DMA_LEN
- 1 个 MSI-X vector (interrupt on DMA-write complete)
- DMA-write only (controller → guest memory)
- splitmix64 LCG (单依赖 = 0；`--seed <u64>` 可复现)

## 启动

```bash
# OpenHCL VTL2 端 (--vsock-port 自选)
cargo run --release --bin pcie_remote_rng_userspace_vsock -- --seed 42 --vsock-port 5005
# guest 端发现一个 PCIe BDF (vendor/device id 暴露的 RNG)
# guest user driver write CTRL bit0=1 → controller 用 splitmix64 填 DMA_LEN bytes
# 到 GPA DMA_GPA → fire MSI-X
```

## 用途

- 当 SDK 普适性 demo
- 写新 PcieDevice 时抄它的 ~360 行作模板

## 参考

- SDK：[`../pcie_remote_userspace_sdk/README.md`](../pcie_remote_userspace_sdk/README.md)
- 同期 NVMe demo：[`../pcie_remote_nvme_userspace/README.md`](../pcie_remote_nvme_userspace/README.md)
- spec：[`../../specs/2026-05-29-pcie-remote-design.md`](../../specs/2026-05-29-pcie-remote-design.md)
