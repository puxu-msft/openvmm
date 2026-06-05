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

# 4. IO 读写（V5e-1 cap = 4 KiB / IO，Linux nvme-cli 默认 bs=4k 单 cmd 完成）
sudo dd if=/dev/zero of=/dev/nvme0n1 bs=4k count=1 oflag=direct
sudo dd if=/dev/nvme0n1 of=/tmp/readback.bin bs=4k count=1 iflag=direct
hexdump -C /tmp/readback.bin | head -3

# 5. 断开
sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace
```

Windows Server 2025 类似（`nvme-cli for Windows` 或 PowerShell `Connect-NvmeoFController`），
spec § 8.13 TCP transport 行为一致。

## V6 AER (Async Event Request)

V6 在 V5 IO base 之上加入 spec § 5.2 AER 完整链路：

- **V6a**：host 发 AER (opc=0x0C) 不再 bail；session 镜像 ≤ 4 (AERL+1) 个 pending
- **V6b**：select-style `pump_one_with_events(tick)` 主循环；新 `inject_aen`
  API 让事件源在 session ctx 内 fire_aen + capture CQE → wire CapsuleResp
- **V6c**：端到端 e2e + 多事件 FIFO 顺序 + 超 pending 时 spec 允许的 drop

### bin 默认行为

`main.rs` 现在用 `pump_one_with_events(100ms)`：每 100 ms 至少调一次
`sync_aer_mirror` 保 controller / session 状态一致。普通 CMD 路径不受
timeout 影响（handshake 后 30s timeout 已 clear，dispatch 期间 stream
timeout = None 不会被 100ms cap 截断 IO Write R2T 等待）。

### 编程接口（V8 timer-driven 事件源用）

```rust
// 当外部 timer 决定温度阈值跨越 / SMART critical 等事件需投递时：
let emitted = session.inject_aen(
    /* aen_type */ 0x01, // 0=Error / 1=SMART / 2=Notice / 7=Vendor (spec 表)
    /* aen_info */ 0x00, // Type-specific info byte
    /* log_id */ 0x02,   // 应被 host 主动 Get Log Page 拉的 log id
)?;
// emitted = 0 → 无 pending AER 可弹（spec 允许 drop）
// emitted = 1 → wire 上已 emit 1 条 CapsuleResp
```

### Linux nvme-cli 真机验证

```bash
# host 端持续监听 AER（dmesg 会出 NVME 事件 log）
sudo dmesg -w &

sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 \
  -n nqn.2026-06.io.openhcl:nvme.userspace

# kernel 默认会自动 post 多条 AER；通过 nvme-cli aer 可显式 post 更多
sudo nvme list

# 触发事件需要 server 端调 inject_aen；当前 bin 没有 SIGUSR1 trigger
# 接口（V6c-followup）。手动 trigger 通过 in-process integration test：
cargo test -p nvme_of_tcp_target --test aer_e2e
```

V8 计划：timer-driven SMART threshold + 通过 ctrlc::set_handler 类机制让
SIGUSR1 触发 inject_aen 用于 manual interop。

## V5 已知限制（V6+ 解除）

| 限制 | 原因 | 解除阶段 |
|---|---|---|
| 单 IO ≤ 8 KiB（nlb ≤ 16 @ LBADS=9，无 PI） | session sentinel scheme 教学版只支持 prp1+prp2 直接指针（≤ 8 KiB）；controller `bytes <= 2*NVME_PAGE_SIZE` 走 dual-PRP path。**session block FORMAT_NVM / NS_MANAGEMENT 防止 host 切换 NS 形状破坏此假设**（V5e-1-fix review H-1） | V5e-3（PRP list path → MDTS 上限）/ V8 PI support |
| 单 backing file 同时仅 1 active connection | 教学版 controller 无 `Arc<Mutex<>>` 共享 | V8.5 |
| **R-8 锁基于 path 字符串**：symlink/hardlink 别名指向同 inode 仍能绕过锁 | clippy 禁 `Path::canonicalize`；inode-based key 需 unix-specific fd metadata | V8（fd-based key 配合 controller 共享） |
| 无 Discovery subsystem | 必须 `nvme connect -n nqn...`，不能 `connect-all` | V7 |
| 无 TLS 1.3 / DH-HMAC-CHAP | spec § 8 独立模块 | V-followup |
| 单 IO queue per session | per-qid 一 TCP conn（与 Linux nvme-tcp 真实行为一致） | V8 |
| nlb > 1 单 IO → driver 自动拆 | 返 SC=0x18 SGL_DATA_LENGTH_INVALID 让 host 重发分片 | V5e |
| 大块 IO 性能差 | 串行多 cmd，无 pipelining | V8 + tokio refactor |
| Ctrl-C / SIGTERM graceful shutdown | 已通过 `ctrlc` crate 实现：flip running flag → accept loop 退 → 等 ≤ 2s in-flight worker drain → exit | V5d-fix-2 ✅ |

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

## V8e — tokio async runtime（KATO timer / AER wakeup / 真并发 e2e）

V8e 系列把整个 stack 重构为 tokio 异步：bin 走 `#[tokio::main]`、async session
通过 `AsyncSession::pump_one_async` 多路复用 `tokio::select!`（shutdown / AER
notify / KATO deadline / read PDU 四 arm）；sync `V2Session` 双轨保留兼容
30+ 老集成测试。

- **V8e-1 [framing async]**：`read_pdu_async` / `write_pdu_async` 与 sync 版
  byte-stream bit-exact（regression gate 锁定）
- **V8e-2 [bin async]**：`#[tokio::main]` + `tokio::sync::watch` shutdown +
  `tokio::net::TcpListener`；ctrlc dep 删，改 `tokio::signal::ctrl_c`
- **V8e-3 [AsyncSession]**：`accept_and_handshake_async` + per-conn
  `pump_one_async`；`SharedControllerInner` 同时被 sync/async 路径共享
- **V8e-4 [AER wakeup]**：`SharedControllerInner.aen_notify: Arc<Notify>` +
  session select! 监听；AER 端到端延迟 ~100ms → < 10ms（spurious wakeup 由
  per-conn 自检过滤）
- **V8e-5 [KATO timer]**：`tokio::time::Sleep` deadline + `set_kato(ms)` /
  `reset_kato_deadline()`；超时 → `PumpEvent::KatoExpired` (spec § 7.13)
- **V8e-6 [e2e]**：4-conn 并发握手 / AER storm 不饥饿 / KATO 续命与 expire
  混合 / shutdown 全 drain
- **V8e-7-1 [dispatch_plan 决策表]**：抽 sans-IO `decide_capsule_kind /
  decide_admin_aer_path / decide_admin_discovery_whitelist /
  decide_admin_blocked_opc / decide_io_nlb_check / prp2_sentinel_for_nlb`
  纯函数，sync/async dispatch 共用决策核（防漂移）
- **V8e-7-2 [fabric handlers async]**：`dispatch_pdu_async` 入口 +
  `handle_connect_async / property_get_async / property_set_async /
  disconnect_async`；KATO reset 在 dispatch 入口统一调；Drop 扩展 IO queue
  sweep
- **V8e-7-3 [admin/IO async + AER drain]**：`handle_admin_cmd_async /
  handle_io_cmd_async / run_post_dispatch_async`（R2T 三段式 lock-pop /
  unlock-await wire / lock-complete）；`drain_aers_async / inject_aen_async`
- **V8e-7-4 [bin 切纯 async]**：每条 conn `tokio::spawn(handle_conn_async)`
  直接走 AsyncSession 全 async path；V8e-2 `spawn_blocking(handle_conn)` 桥
  退役（sync `handle_conn` 保留作 V8b/c/d/f 集成测试 + V-followup 参考）

### Linux nvme-cli 真使用 (V8e-7 后)

```bash
# 启 bin（kernel ≥ 5.0 nvme-tcp 模块；--keep-alive-tmo 单位 s）
target/release/nvme_of_tcp_target --listen 127.0.0.1:4420 \
    --backing-file /tmp/disk.img

# host 端：4 IO queue 并发 + KATO=10s（spec § 7.13）
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 \
    -n nqn.2014-08.org.nvmexpress:teaching:disk \
    -i 4 --keep-alive-tmo 10

# 验：lsblk 列出 /dev/nvmeXn1；fio 4 jobs 真并发
sudo fio --name=v8e --rw=randread --bs=4k --iodepth=1 --numjobs=4 \
    --runtime=10 --filename=/dev/nvme0n1
```

V8e-7-4 后 bin 端 4 conn 真并发 IO 全走 `tokio::spawn` async path（无
spawn_blocking thread pool 阻塞）；controller 端仍 `parking_lot::Mutex` 短锁
串行化（plan §3 Q1 决策；async-aware lock 留 V-followup），但 wire 层 4 conn
真并发握手 + AER + KATO timer 全 tokio 调度。

## TLS 教学开关（V-followup-tls）

> ⚠️ **教学版警示**：本 TLS 实现仅适合 dev / lab / CTF / 教学。
> 它 **不做** cert chain validation，也 **不做** host NQN ↔ TLS identity
> binding（spec § 8.13 强制要求）。生产请等：
> - **V-followup-auth** — NQN ↔ TLS identity 绑定
> - **V-followup-mtls** — 强制 client cert
> - **V-followup-tls-PSK** — TP-8011 PSK + HKDF-Expand-Label NVMe labels
> - **DH-HMAC-CHAP** — in-band auth (spec § 8.13.5)

### 启用步骤

1. **生成自签 cert + key**（仅教学；生产请用 CA 签发并配 chain）：

   ```bash
   # rcgen-cli / openssl 都可；下例用 openssl
   openssl req -x509 -newkey ec:<(openssl ecparam -name prime256v1) \
       -keyout server.key -out server.pem \
       -days 7 -nodes -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost"
   chmod 600 server.key
   ```

2. **启 bin（plaintext + TLS dual-listener）**：

   ```bash
   nvme_of_tcp_target \
       --listen 127.0.0.1:4420 \
       --tls-listen 127.0.0.1:8009 \
       --tls-cert server.pem \
       --tls-key  server.key \
       --tls-i-trust-this-cert \
       --backing-file disk.img
   ```

   - `--tls-listen` 必须配 `--tls-cert + --tls-key + --tls-i-trust-this-cert`
   - 缺一即 hard-fail 并打中文红色 stderr 解释
   - TLS handshake 30s 超时防 ClientHello slowloris (`TLS_HANDSHAKE_TIMEOUT_SECS`)
   - TLS port 不 fallback plaintext（plan R-5 防 downgrade）

3. **client（Linux nvme-cli ≥ 2.6 支持 `--tls`）**：

   ```bash
   sudo nvme connect -t tcp -a 127.0.0.1 -s 8009 \
       -n nqn.2014-08.org.nvmexpress:teaching:disk \
       --tls --tls-key=server.key
   ```

### 设计要点

- **stream 抽象泛化** (V-followup-tls-1)：`AsyncSession<S>` 接任意
  `AsyncRead + AsyncWrite + Unpin + Send + 'static`；既有 `TcpStream` /
  in-memory `DuplexStream` / 新的 `TlsStream<TcpStream>` 全部 0 改动复用
- **TlsAcceptor 构造** (V-followup-tls-2)：`build_acceptor_from_pem` 接受
  X.509 cert chain + PKCS#8 / PKCS#1 / SEC1 key，rustls 0.23 + tokio-rustls
  0.26，默认 ring crypto provider
- **dual-listener** (V-followup-tls-3)：plaintext + TLS 共享同 controller 同
  shutdown watch；TLS 端 monomorphize 给 `AsyncSession<TlsStream<TcpStream>>`
- **byte-identical gate** (V-followup-tls-4)：TLS 路径与 plaintext 路径对同样
  PDU 输入序列产生**解密后字节完全等价的应用层 wire**；防未来给 TLS 偷偷加
  in-band transformation

### TLS-related caveat 清单

- ❌ 不验 client cert（mTLS 留 V-followup-mtls）
- ❌ 不绑 host NQN ↔ TLS identity（spec § 8.13 强制要求；留 V-followup-auth）
- ❌ 不强制 TLS 1.3（教学版接 TLS 1.2 让老 client 可连；生产应用
  `with_protocol_versions(&[&rustls::version::TLS13])`）
- ❌ 不做 cert chain validation（self-signed 直通）
- ❌ 不做 PSK / DH-HMAC-CHAP
- ✅ TLS handshake 30s 超时防 slowloris
- ✅ TLS handshake 失败不 fallback plaintext（防 downgrade）
- ✅ 双 explicit consent CLI flag 强制
- ✅ `#![forbid(unsafe_code)]` 保留；rustls 内部 unsafe 由 upstream audit

## mTLS 教学开关（V-followup-mtls）

强制 client 出示 cert，且 chain 必须 anchor 到你提供的 CA bundle。
在 V-followup-tls-3 的四参基础上加 **第五个** flag：

```bash
nvme_of_tcp_target \
    --listen 127.0.0.1:4420 \
    --tls-listen 127.0.0.1:8009 \
    --tls-cert server.pem \
    --tls-key  server.key \
    --tls-i-trust-this-cert \
    --tls-client-ca client-ca.pem \
    --backing-file disk.img
```

行为：
- 一旦设 `--tls-client-ca`，acceptor 从 `build_acceptor_from_pem` 切到
  `build_acceptor_with_mtls`
- client 不带 cert / cert 不在 CA bundle 信任链 → TLS handshake fail
- mTLS 路径仍 **不** 绑 NQN identity（spec § 8.13 强制要求，留 V-followup-auth）

### 生成 client CA 与 client cert（教学）

```bash
# 1. 生成 client CA（自签）
openssl req -x509 -newkey ec:<(openssl ecparam -name prime256v1) \
    -keyout client-ca.key -out client-ca.pem \
    -days 30 -nodes -subj "/CN=client-ca"

# 2. 签发 client cert
openssl req -new -newkey ec:<(openssl ecparam -name prime256v1) \
    -keyout client.key -out client.csr \
    -nodes -subj "/CN=host-1"
openssl x509 -req -in client.csr -CA client-ca.pem -CAkey client-ca.key \
    -out client.pem -days 30 -CAcreateserial

# 3. host 端 nvme-cli (≥ 2.6 支持 --tls-client-key)
sudo nvme connect -t tcp -a 127.0.0.1 -s 8009 \
    -n nqn.2014-08.org.nvmexpress:teaching:disk \
    --tls --tls-key=client.key --tls-keyring=client.pem
```

### mTLS caveat 清单

- ✅ 强制 client 出示 cert
- ✅ 强制 cert chain anchor 到 `--tls-client-ca`
- ❌ 仍 **不** 绑 NQN ↔ client cert identity（V-followup-auth 才做）
- ❌ 不做 cert revocation 检查（CRL / OCSP；留生产版）
- ❌ 不做 SAN / CN whitelist
- ✅ `WebPkiClientVerifier` 由 rustls 默认 webpki crate 实现，audit 自动覆盖

## 内部参考

- 计划文档：[`../../plans/2026-06-04-phase-v-nvme-of-tcp.md`](../../plans/2026-06-04-phase-v-nvme-of-tcp.md)
  + [V4 详细计划](../../plans/2026-06-05-phase-v4-detailed.md)
  + [V5 详细计划](../../plans/2026-06-05-phase-v5-detailed.md)
  + [V8 详细计划](../../plans/2026-06-06-phase-v8-detailed.md)
  + [V8e tokio refactor 详细计划](../../plans/2026-06-06-phase-v8e-tokio-detailed.md)
  + [V8e-7 dispatch 详细计划](../../plans/2026-06-06-phase-v8e-7-dispatch-detailed.md)
- wire spec：[`../../specs/2026-06-04-nvme-tcp-wire-reference.md`](../../specs/2026-06-04-nvme-tcp-wire-reference.md)
- backend controller：[`../pcie_remote_nvme_userspace/README.md`](../pcie_remote_nvme_userspace/README.md)

## 许可

MIT — 与上游 openvmm 仓库一致。
