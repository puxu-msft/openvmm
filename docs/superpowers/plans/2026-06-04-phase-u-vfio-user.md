# Phase U — vfio-user Backend

> **Status:** design draft（执行前再细化）
> **Date:** 2026-06-04
> **Prereq:** Phase T（`trait Transport` 抽出）
> **Goal:** 让同一份 `NvmeController` 通过 vfio-user UNIX socket 直接被 QEMU 接管，
> 命令行 `qemu -device vfio-user-pci,socket=/tmp/nvme.sock` 即可挂载。

## 1. 动机
- 行业标准协议（QEMU 8+/Cloud Hypervisor/SPDK）
- 与 OpenHCL pcie_remote 共享同一份 NVMe 控制器代码，验证 Phase T 抽象
- WSL/CI 上无需 OpenHCL/Hyper-V 即可端到端跑

## 2. 架构

```
                      trait Transport （Phase T 抽出）
                              ▲
                ┌─────────────┴────────────────┐
        OpenhclVsockTransport            VfioUserTransport
        （existing pcie_remote）          （NEW Phase U，UNIX socket）
                ▲                              ▲
                │                              │
        OpenHCL VTL2 shim                qemu-system-x86_64
                                         -device vfio-user-pci,socket=…
```

## 3. 新 crate

```
docs/superpowers/examples/pcie_vfio_user_sdk/  ── NEW
  Cargo.toml   deps: pcie_device_sdk + nix (fd-passing) + bytes
                     + futures + anyhow + tracing （NO tokio NO libvfio-user）
  src/lib.rs                 mod 声明 + serve_unix(path, factory)
  src/proto/header.rs        16-byte msg header serde
  src/proto/version.rs       VERSION handshake + capabilities JSON
  src/proto/device_info.rs   GET_INFO / GET_REGION_INFO / GET_IRQ_INFO
  src/proto/region_io.rs     REGION_READ/WRITE
  src/proto/dma.rs           DMA_MAP/UNMAP（SCM_RIGHTS recv）+ DMA_READ/WRITE
  src/proto/irq.rs           SET_IRQS（recv eventfd）+ fire_interrupt
  src/transport.rs           impl Transport for VfioUserTransport
  src/server.rs              UnixListener accept loop
  README.md                  QEMU cmdline + WSL caveats
```

预估 ~1.6 KLOC（impl 1300 + tests 300）。

## 4. Message → SDK 映射

| vfio-user msg | 方向 | 用法 | 实现 |
|---|---|---|---|
| `VERSION` (1) | bidi | 版本 + capabilities JSON | `VfioUserTransport::handshake()`，v0.1 |
| `DEVICE_GET_INFO` (3) | C→S | device flags + region/irq 数 | `flags=PCI num_regions=9 num_irqs=5` |
| `DEVICE_GET_REGION_INFO` (4) | C→S | 每 region 的 size/offset/flags | 查 BAR 表；BAR0 = 8 KiB |
| `REGION_READ` (7) | C→S | guest MMIO read | → `device.mmio_read(bar,offset,size)` |
| `REGION_WRITE` (8) | C→S | guest MMIO write | → `device.mmio_write(...)` |
| `DMA_MAP` (9) | C→S | QEMU 报告 guest RAM region | 存 metadata；不 mmap（message DMA） |
| `DMA_UNMAP` (10) | C→S | RAM 撤映射 | 删表 |
| `DMA_READ` (11) | **S→C** | controller 读 guest mem | `dma_read(token, gpa, len)` → send DMA_READ req → 等 reply → msg_id ↔ token map → 回 `on_dma_complete` |
| `DMA_WRITE` (12) | **S→C** | controller 写 guest mem | 同上 |
| `DEVICE_SET_IRQS` (14) | C→S | MSI-X eventfd 配置 | 存 `Vec<Option<File>>`；`fire_interrupt(vec)` 写 8B u64=1 |
| `DEVICE_RESET` (13) | C→S | PCIe FLR | → `device.reset(kind=FLR)` |
| `DEVICE_GET_IRQ_INFO` (5) | C→S | 查 IRQ 最大向量 | MSIX = msix_count |
| `DIRTY_PAGES` (24) | C→S | live migration | Phase U 外，reply `EINVAL` |

## 5. DMA 模式抉择

vfio-user 支持 (a) 共享 memfd 零拷贝 (b) 消息化 DMA_READ/WRITE。

**Phase U：仅消息化**。理由：
- 与现有 `DeviceCtx` token model 1:1 对齐（pcie_remote 也是协议化 DMA）
- 无 mmap / GPA→HVA 翻译 / fd 表 OS 资源管理
- 性能足够教学；零拷贝优化留 Phase V/W

> **待验证**：spec 是否强制 server `mmap()` 收到的 fd？若是仍要 mmap 但不用 pointer。

## 6. 同步 vs 异步 DMA reply

vfio-user `DMA_READ` 是 server→client request，reply 异步到达 ——
与 `DeviceCtx` 的 token-based async DMA 完美对齐。

`VfioUserTransport` 内 `HashMap<msg_id, dma_token>`：发请求时 insert；
收 reply 时 remove → 喂给主循环的 `on_dma_complete` 派发路径。
**不需要新增 `dma_read_sync`**。

## 7. Rust 生态决策

| 候选 | 评估 |
|---|---|
| `vfio-user` crate (cloud-hypervisor) | 与 CH 内部 fd table 强耦合，trait 不对齐 |
| crates.io `vfio-user` 0.x | 维护少，trait 不匹配 |
| libvfio-user (C) + FFI | 否决：`#![forbid(unsafe_code)]` + C 编译依赖 + Linux only |
| **自研 ~1.6 KLOC** | ✅ 首选 |

自研理由：spec 简单（~20 个 msg，用到 ~10）、零 unsafe、clean fit、教学。

## 8. 阶段化实施

### U1 — proto 数据结构 + 单测
每 message 一个 roundtrip + 一个 error case，~25 单测。

### U2 — UNIX socket + VERSION handshake
`serve_unix(path, device_factory)` 入口。`UnixStream::pair()` 端到端 handshake。

### U3 — REGION_READ/WRITE 接 PcieDevice
Python libvfio-user 客户端验：`device_get_region_info` + `region_read(0,4)` 拿 vendor ID。

### U4 — DMA_MAP/UNMAP + DMA_READ/WRITE
fd-passing + DmaRegion 表；msg_id↔token map；4 个 failure path（GPA 越界 / msg_id 未知 /
reply error flag / socket EOF）。

### U5 — SET_IRQS + 端到端 QEMU demo
eventfd trigger；CLI 加 `--vfio-user-sock`；QEMU + Linux guest 见 `nvme0n1`，
`mkfs.ext4 + mount + dd 800M` 跑通。

## 9. QEMU cmdline（参考）

```bash
# Term 1
./target/release/pcie_remote_nvme_userspace \
    --vfio-user-sock /tmp/nvme.sock \
    --backing-file /tmp/ns1.img

# Term 2
qemu-system-x86_64 -enable-kvm -m 1G -smp 2 \
    -kernel /boot/vmlinuz -append "root=/dev/nvme0n1 console=ttyS0" \
    -initrd /boot/initrd \
    -object memory-backend-memfd,id=mem,size=1G,share=on \
    -numa node,memdev=mem \
    -device vfio-user-pci,socket=/tmp/nvme.sock \
    -nographic
```

`memory-backend-memfd,share=on` + `-numa node,memdev=mem` 是 QEMU vfio-user 强制要求
（即使 server 不 mmap）。WSL2 KVM 可用。

## 10. 测试

- **Unit（每 phase 强制）**：header roundtrip / opcode dispatch / token routing / capability JSON 容错
- **Integration A**：libvfio-user Python 客户端（事实标准合规测试）—— manual gate
- **Integration B**：~200 行自写 Rust vfio-user client 在 `tests/`，pre-canned fixture
- **Manual E2E**：QEMU + Linux guest 完整 IO 链
- **回归**：现有 67 NVMe + 7 SDK 全过 + OpenHCL Hyper-V e2e

## 11. 风险

| 风险 | 严重 | 缓解 |
|---|---|---|
| QEMU client 与 spec 偏差 | M | 用 QEMU 9.0+；对照 `hw/remote/vfio-user-obj.c` |
| WSL2 fd-passing 兼容 | M | 5 行 `nix` echo 预测 |
| `pal_async` UnixStream 支持 | M | 已有 `PolledSocket<socket2::Socket>` 用例 |
| Phase T 阻塞 | **H** | 必须先完成 |
| `forbid(unsafe_code)` + `nix` fd-passing | L | nix 内部 unsafe，我们 call safe API |

## 12. 开放问题（实施期决）

1. spec 是否强制 server `mmap()` 收到的 fd？
2. SET_IRQS 重新配置时是否要求 close 旧 fd？
3. QEMU `vfio-user-pci` 是否强制 CONFIG region？
4. 是否需要 `DEVICE_FEATURE`？Phase U 否决。

## 13. 不在 Phase U 范围

- Shared memfd zero-copy DMA
- Live migration / dirty pages
- Cloud Hypervisor client 兼容
- vsock transport for vfio-user
- 多客户端并发（vfio-user spec: 1 client/socket）
- TLS / 自定义认证

## 14. Acceptance

- [ ] `pcie_vfio_user_sdk` cargo build + clippy 0 warning
- [ ] 单测 ≥ 30，覆盖 7 个 message family
- [ ] CLI `--vfio-user-sock` 启动 + QEMU + Linux guest 见 NVMe 盘 + mkfs + 大文件 IO
- [ ] 现有 67 NVMe + 7 SDK + OpenHCL e2e 不回归
- [ ] README 含 QEMU cmdline + WSL caveats
- [ ] rust-reviewer 0 CRITICAL/HIGH；security-reviewer 0 CRITICAL（fd-passing）

## 附录 A：参考

- libvfio-user spec: https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst
- libvfio-user header: https://github.com/nutanix/libvfio-user/blob/master/include/vfio-user.h
- QEMU client: hw/remote/vfio-user-obj.c
- Cloud Hypervisor vfio-user: https://github.com/cloud-hypervisor/cloud-hypervisor/tree/main/vfio_user
- SPDK NVMe-over-vfio-user: https://spdk.io/doc/nvmf.html
