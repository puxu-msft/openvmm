# scripts/interop_py — vfio-user 跨进程实证 harness

> **stdlib only，0 sudo / 0 QEMU 依赖**。以真 vfio-user 客户端身份连入 server 的
> UNIX socket，按真 wire 协议驱动，catch loopback Rust 单测看不到的跨进程 wire
> bug（见 [LESSONS §7](/usnvmemu/docs/LESSONS.md)）。

## 为什么是 Python 而非真 QEMU

真 QEMU vfio-user e2e 需要带 vfio-user 的 QEMU 构建 + guest 镜像，开发/CI 环境
不一定有。沿用 [`nvme_of_tcp_target/scripts/interop_py`](/usnvmemu/crates/nvme_of_tcp_target/scripts/interop_py/README.md)
的成熟模式：一个 Python stdlib vfio-user **客户端**跨进程驱动 server，验证 wire
正确性。Python `socket.sendmsg` 还支持 SCM_RIGHTS，将来 mmap DMA fd 路径也能测。

## 用法

```bash
cd crates/vfio_user_transport/scripts/interop_py
python3 enumerate_smoke.py      # 自动 build + spawn server，无需手动起
# 或 uv：uv run python enumerate_smoke.py
```

退出码 0 = 全 scenario 通过。脚本自包含（build nvme_firmware + spawn
`--vfio-user-sock` server + 连入 + 校验 + 清理）。

## Script 一览

| Script | 验证 | 关键断言 |
|--------|------|---------|
| `enumerate_smoke.py` | PCI 枚举 + **W1 cfg-space 真路由** + 描述模型统一 | VERSION 握手 / GET_INFO(9 region,5 irq) / GET_REGION_INFO(CONFIG 4096, BAR0 8 KiB describe-derived) / **REGION_READ CONFIG → vendor 0x1414 / device 0xc0de / class 01.08.02**（此前 bug：CONFIG 误当 BAR MMIO 读不到 identity）/ GET_IRQ_INFO(MSI-X=4) / BAR size-probe(0xFFFFE000) / FLR 后 config 复位 |

`vfio_proto.py` 是可复用 wire helper 模块（Header `<HHIII` / Command 常量 /
send/recv/expect_reply / version_handshake / get_info / get_region_info /
get_irq_info / region_read/write / device_reset）。

## 后续 scenario（待加）

- DMA head-of-line 跨进程版：DMA_MAP + doorbell 触发 server DMA_READ + 等待期插
  REGION_READ，验连接不挂（当前已有 Rust 单测 `dma_read_sync_defers_interleaved_inbound`）。
- mmap DMA：DMA_MAP 带 memfd（`sendmsg` SCM_RIGHTS）+ 验零拷贝路径（待 mmap 实现）。
