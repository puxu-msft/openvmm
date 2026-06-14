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

## libvfio-user 官方 client（differential oracle，gold standard）

Python harness 与我们 server 同源（可能共享错误假设）。**真正的 spec-conformance
oracle 是 [libvfio-user](https://github.com/nutanix/libvfio-user)（Nutanix 官方 C
实现）的 `samples/client`** —— 独立第二实现，catch self-consistent 测试看不到的 bug
（LESSONS §2）。真 QEMU 8.2 主线**无** vfio-user 客户端（只有 vhost-user），故 libvfio-user
client 是这里能跑的最强验证。

复现：
```bash
sudo apt install -y libjson-c-dev libcmocka-dev      # libvfio-user 构建依赖
git clone --depth 1 https://github.com/nutanix/libvfio-user.git /tmp/libvfio-user
cd /tmp/libvfio-user && meson setup build && ninja -C build
# 起我们的 server，再用官方 client 连入：
nvme_firmware --vfio-user-sock /tmp/x.sock --backing-file /tmp/x.img &
/tmp/libvfio-user/build/samples/client /tmp/x.sock
```

**已自动化（2026-06-14，ADR-013 vfio Tier B）**：上述手动复现已固化为 runner
`run_libvfio_differential.sh`（pinned libvfio-user commit + 自动断言"bogus-region EINVAL +
到达 vid 断言"= 协议前缀差分通过）+ opt-in/cadence CI job `.github/workflows/usnvmemu-libvfio-differential.yml`
（非 hermetic 故不入 always-on gate）。revert-verify 实证：注入 region-info 回归 → runner FAIL。

**已验证（官方实现确认我们的协议层）**：VERSION 握手 / bogus-region → EINVAL /
GET_DEVICE_INFO(9 region, 5 irq) / GET_REGION_INFO 全 9 region 尺寸+flags /
bulk config-space read。

**oracle 抓到并已修的 2 个真 conformance bug**（commit `533432ec`）：
1. bogus region 访问应 EINVAL（此前不校验 region index → 误返 success）。
2. bulk REGION_READ（config header 一次读 64 字节；此前 1/2/4/8 限制太严）。

**已知 diverge（非通用 spec，不追）**：client 之后断言 identity == `0xdead/0xbeef/
0xcafe/0xbabe`（它写死 libvfio-user 自带 sample server 的身份，我们是 NVMe
`0x1414/0xc0de`），再之后用 `VFIO_USER_DEVICE_FEATURE`（dirty-page 迁移，我们未实现）。
这些是 sample-server-specific / 高级 migration feature，不是协议枚举层的合规缺口。

## 后续 scenario（待加）

- DMA head-of-line 跨进程版：DMA_MAP + doorbell 触发 server DMA_READ + 等待期插
  REGION_READ，验连接不挂（当前已有 Rust 单测 `dma_read_sync_defers_interleaved_inbound`）。
- mmap DMA：DMA_MAP 带 memfd（`sendmsg` SCM_RIGHTS）+ 验零拷贝路径（待 mmap 实现）。
