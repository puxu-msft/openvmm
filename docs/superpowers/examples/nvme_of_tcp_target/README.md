# nvme_of_tcp_target

**Phase V5d** — NVMe-over-Fabrics TCP target backed by
[`pcie_remote_nvme_userspace`](../pcie_remote_nvme_userspace/) `NvmeController`。
Linux ≥ 5.0 / Windows Server 2025 上的标准 `nvme-cli` 可通过
`nvme connect -t tcp` 直接挂载并跑 IO。

## Status

**当前进度：V5a/b/c/d 全部完成；admin + IO Read + IO Write 端到端 work。**

- ✅ V0 — controller lib+bin 拆分
- ✅ V1 — PDU 编解码 + CRC32C digest + 同步 TCP framing
- ✅ V2 — ICReq/ICResp 握手 + Fabric Connect / Property Get/Set
- ✅ V3 — admin cmd (Identify / Get Log / Set Features) 闭环
- ✅ V4a — wire layer (R2T / H2CData reassembler / TTAG allocator)
- ✅ V4b — controller dma_read → R2T → H2CData (单段 ≤ 64 KiB)
- ✅ V4c — MAXH2CDATA 分片 + 多 R2T 串行（admin path）
- ✅ V5a — IO queue 安装 + Fabric Connect qid≥1 + dispatch 二分
- ✅ V5b — IO Read 走 C2HData
- ✅ V5c — IO Write 走 R2T/H2CData（含数据持久化验证）
- ✅ **V5d — `main.rs` TcpListener 入口 + README + bin smoke test**
- ⏳ V6 — AER delivery
- ⏳ V7 — Discovery subsystem (Log Page 0x70)
- ⏳ V8 — Multi-queue per session + 完整 Disconnect

## ⛔ Security warning

**本 bin 无 TLS / 无 in-band auth (DH-HMAC-CHAP) / 无 host NQN 白名单**。
任何能到达监听端口的 host 都能远程读写所有 `--backing-file` 内容。

- 默认 `--listen 127.0.0.1:4420` 只接受同机 connection
- 非 loopback 地址（例 `0.0.0.0` / `192.168.x.x`）需显式 `--i-know-this-is-insecure`
  才会启动；启动后 stderr 出 prominent WARN
- 生产 / 共享 LAN 部署务必加 IP 层 ACL / WireGuard / Tailscale 隧道
- 教学 / 本机演示推荐保留默认 loopback

V-followup 计划加 TLS 1.3 + DH-HMAC-CHAP。

## Build & Run

```bash
cd docs/superpowers/examples/nvme_of_tcp_target
truncate -s 1G /tmp/ns1.img         # 1 GiB 空 backing file
cargo run --release -- \
  --listen 127.0.0.1:4420 \
  --backing-file /tmp/ns1.img \
  --vid 0x1414
```

CLI 选项：

| Flag | Default | 说明 |
|---|---|---|
| `--listen` | `127.0.0.1:4420` | 监听地址；非 loopback 需 `--i-know-this-is-insecure` |
| `--backing-file PATH` | （必需） | 后端文件；重复指定 → 多 namespace（nsid 按命令行顺序 1, 2, …） |
| `--vid 0xNNNN` | `0x1414` | PCIe Vendor ID（教学示例值，非 Microsoft 官方组件） |
| `--ssvid 0xNNNN` | `0x0000` | Subsystem Vendor ID |
| `--zns-nsid N` | （重复可选） | 把指定 NSID 标记为 ZNS（Zoned Namespace） |
| `--max-connections N` | `16` | 并发连接上限（防 thread/fd 资源耗尽） |
| `--i-know-this-is-insecure` | `false` | 显式确认绑非 loopback 地址 |

环境变量：默认 `RUST_LOG=info`；用 `RUST_LOG=debug` 看每条 PDU、`warn` 静音。

## Linux nvme-cli interop

```bash
# 1. host 加载内核模块（kernel ≥ 5.0）
sudo modprobe nvme_tcp

# 2. 连接（指定 NQN）
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 \
  -n nqn.2026-06.io.openhcl:nvme.userspace

# 3. 列出 / Identify
sudo nvme list                    # 期望见 /dev/nvme0n1
sudo nvme id-ctrl /dev/nvme0      # VID = 0x1414
sudo nvme id-ns /dev/nvme0n1

# 4. IO 读写
sudo dd if=/dev/zero of=/dev/nvme0n1 bs=512 count=1 oflag=direct
sudo dd if=/dev/nvme0n1 of=/tmp/readback.bin bs=512 count=1 iflag=direct
hexdump -C /tmp/readback.bin | head -3

# 5. 断开
sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace
```

Windows Server 2025 类似（`nvme-cli for Windows` 或 PowerShell `Connect-NvmeoFController`），
spec § 8.13 TCP transport 行为一致。

## V5 已知限制（V6+ 解除）

| 限制 | 原因 | 解除阶段 |
|---|---|---|
| 单 IO ≤ 512 byte（nlb=1） | session sentinel scheme 教学版只支持单 PRP1 | V5e（多 PRP / PRP list） |
| 单 backing file 同时仅 1 active connection | 教学版 controller 无 `Arc<Mutex<>>` 共享 | V8.5 |
| 无 Discovery subsystem | 必须 `nvme connect -n nqn...`，不能 `connect-all` | V7 |
| 无 TLS 1.3 / DH-HMAC-CHAP | spec § 8 独立模块 | V-followup |
| 单 IO queue per session | per-qid 一 TCP conn（与 Linux nvme-tcp 真实行为一致） | V8 |
| nlb > 1 单 IO → driver 自动拆 | 返 SC=0x18 SGL_DATA_LENGTH_INVALID 让 host 重发分片 | V5e |
| 大块 IO 性能差 | 串行多 cmd，无 pipelining | V8 + tokio refactor |

## 架构概览

```text
Linux host (nvme-tcp driver)
       ↓ TCP :4420
nvme_of_tcp_target (bin)
       ↓ per-conn thread
V2Session ─── pump_one loop ───┐
       │                       │
       │ CapsuleCmd 派发        │ R2T/H2CData/C2HData wire
       ↓                       ↓
NvmeController (admin/IO)  framing.rs (CRC32C digest)
       ↓
backing file (regular file)
```

session 关键状态：
- `current_qid: u16` — 单 conn 单 qid（per Linux nvme-tcp 真实行为）
- `io_queues: HashMap<u16, IoQueueState>` — admin Create IO CQ/SQ 后镜像记账
- `next_token: u64` — TcpAdminTransport token 跨 cmd 单调（修 V3-polish review M-1）
- `ttag_alloc: TtagAllocator` — R2T transfer tag，wrap 跳过 0

控制 / 数据流：
- **admin / IO Read**：`controller.dma_write(prp1_sentinel, buf)` →
  session captured.writes → C2HData PDU + CapsuleResp
- **IO Write / admin dma_read（如 NS Attachment 0x15）**：
  `controller.dma_read(prp1_sentinel, len)` → session captured.pending_reads
  → R2T (per-MAXH2CDATA chunk) → H2CData → on_dma_complete → post_cqe →
  captured CQE write → CapsuleResp

## 测试

```bash
# Unit + integration（in-process tcp_pair）
cargo test -p nvme_of_tcp_target

# bin smoke（spawn 子进程 + ICReq handshake）
cargo test -p nvme_of_tcp_target --test bin_smoke

# 完整套（含 controller backend）
cd ../pcie_remote_nvme_userspace && cargo test --lib
```

预期 78 + 1 + 67 = 146 测试全 green。

## 内部参考

- 计划文档：[`../../plans/2026-06-04-phase-v-nvme-of-tcp.md`](../../plans/2026-06-04-phase-v-nvme-of-tcp.md)
  + [V4 详细计划](../../plans/2026-06-05-phase-v4-detailed.md)
  + [V5 详细计划](../../plans/2026-06-05-phase-v5-detailed.md)
- wire spec：[`../../specs/2026-06-04-nvme-tcp-wire-reference.md`](../../specs/2026-06-04-nvme-tcp-wire-reference.md)
- backend controller：[`../pcie_remote_nvme_userspace/README.md`](../pcie_remote_nvme_userspace/README.md)

## 许可

MIT — 与上游 openvmm 仓库一致。
