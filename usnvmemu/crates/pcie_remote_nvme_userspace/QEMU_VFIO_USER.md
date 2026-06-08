# NVMe Userspace Controller — QEMU vfio-user 接管模式

> **Phase U-followup** — 让本 NVMe controller 通过 [vfio-user 协议][spec]
> 被 QEMU `-device vfio-user-pci,socket=...` 直接接管，无需 OpenHCL / Hyper-V。
> 跨 hypervisor 复用同一份 controller 代码。
>
> [spec]: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst

## 用法

### 1. 启 controller server

```bash
# 准备 backing file
fallocate -l 1G /tmp/nvme_ns1.img

# 启 vfio-user UNIX socket server（不是 vsock/TCP）
./target/release/pcie_remote_nvme_userspace \
    --vfio-user-sock /tmp/nvme.sock \
    --backing-file /tmp/nvme_ns1.img
```

server 会绑定 `/tmp/nvme.sock`、accept 单个 QEMU client，跑握手 + 命令派发 loop。
peer 断开后重 accept；factory 每次 fresh 一个 NvmeController（backing file
持久化数据自然保留）。

### 2. QEMU 接管

```bash
qemu-system-x86_64 -enable-kvm -m 1G -smp 2 \
    -kernel /boot/vmlinuz-$(uname -r) \
    -initrd /boot/initrd.img-$(uname -r) \
    -append "root=/dev/nvme0n1 console=ttyS0,115200" \
    -object memory-backend-memfd,id=mem,size=1G,share=on \
    -numa node,memdev=mem \
    -device vfio-user-pci,socket=/tmp/nvme.sock \
    -nographic
```

> **强制**：`memory-backend-memfd` + `share=on` + `-numa node,memdev=mem`。
> QEMU vfio-user-pci 要求 guest RAM 通过 memfd 暴露给 server（即使 server
> 不 mmap 也要满足这个 setup）。WSL2 KVM 可用。

### 3. guest 验证

```bash
# guest 内
nvme list
nvme id-ctrl /dev/nvme0
lsblk
mkfs.ext4 /dev/nvme0n1
mount /dev/nvme0n1 /mnt
dd if=/dev/urandom of=/mnt/test bs=4M count=200 oflag=sync
nvme smart-log /dev/nvme0
```

## 当前实现状态

| vfio-user 协议命令 | 状态 |
|---|---|
| VERSION（握手 + caps JSON） | ✅ |
| DEVICE_GET_INFO | ✅ flags=PCI\|RESET, num_regions=9, num_irqs=5 |
| DEVICE_GET_REGION_INFO | ✅ BAR0=8KiB R/W, CONFIG=4KiB R/W, 其它 size=0 |
| DEVICE_GET_IRQ_INFO | ✅ MSI-X = msix_count, 其它 0 |
| REGION_READ / REGION_WRITE | ✅ → PcieDevice::mmio_read/write |
| DMA_MAP / DMA_UNMAP | ✅ 表跟踪 + bound/perm 校验；fd 忽略（消息化 DMA） |
| DMA_READ / DMA_WRITE | ✅ server-initiated 同步往返 + on_dma_complete 投递 |
| DEVICE_SET_IRQS | ✅ DATA_EVENTFD+TRIGGER / DATA_NONE+TRIGGER；MASK/UNMASK best-effort |
| DEVICE_RESET | ✅ → PcieDevice::reset(0) |
| DEVICE_GET_REGION_IO_FDS | ⛔ ENOTSUP（我们不 mmap region） |
| migration / dirty pages | ⛔ 不实现 |

## 已知限制（教学版）

- **单 client / 单 socket**：vfio-user spec 默认；多客户端用 Phase U2-followup
- **消息化 DMA**：QEMU 通告的 mmap fd 被忽略；DMA 全走 message round-trip。
  性能比 shared-memfd 路径慢约 1 个数量级，但教学清晰、零 mmap 安全负担
- **client 在等 DMA reply 期间不能插 inbound cmd**（单 socket 同步模型；
  QEMU 实际不这么做，但 spec 允许 — U-followup 多路复用可补）
- **MSI-X capacity**：advertise `max_msg_fds=128`，单 `SET_IRQS` 可传 128 向量
- **fd 数与 count 不匹配** → EINVAL（避免中断丢）

## 与 OpenHCL pcie_remote 路径对比

| 维度 | `--vm-id` (OpenHCL pcie_remote) | `--vfio-user-sock` (本路径) |
|---|---|---|
| transport | AF_HYPERV vsock 或 TCP loopback | UNIX socket |
| hypervisor | OpenVMM/Hyper-V VTL2 + OpenHCL shim | 任意 vfio-user 兼容客户端（QEMU 8+/CH/SPDK） |
| 协议 | 自定义 protobuf | 业界 vfio-user 标准 |
| 测试 | WSL → Windows guest | 本地 KVM Linux guest |
| 主要受众 | OpenHCL 用户态设备开发 | 跨 hypervisor 教学 + interop |

控制器 *本体* 代码 100% 共享（Phase T transport 抽象的成果）。
