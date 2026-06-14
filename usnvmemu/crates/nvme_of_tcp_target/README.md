# nvme_of_tcp_target

**V-followup-dhchap-4d + V-followup-tls-psk (TP-8011) 全栈** — NVMe-over-Fabrics TCP target backed by
[`nvme_firmware`](/usnvmemu/crates/nvme_firmware/) `NvmeController`。
Linux ≥ 5.0 上的标准 `nvme-cli`（`nvme connect -t tcp`）或 **Windows Server 2025 inbox initiator**
（`stornvmeofi.sys` / `nvmeofutil connect`）可直接挂载并跑 IO。**2026-06-14 真 WS2025 实测出盘**
（static controller model + transport-SGL Connect，见 Status / [NVME_OF_TCP.md §7](/usnvmemu/docs/NVME_OF_TCP.md)）。

> **本 README 是 build/run/interop 使用手册**；想先建立这条线的**全貌**（是什么 / 数据流 / 关键模型 /
> 教学-生产边界 / 文档地图）请读自上而下导览 **[NVME_OF_TCP.md](/usnvmemu/docs/NVME_OF_TCP.md)**。

> **持续开发的顶层文档** (新加 phase 前必读):
> - **[NVME_OF_TCP.md](/usnvmemu/docs/NVME_OF_TCP.md)** — 本线自上而下导览 (reader's guide)：全貌入口
> - **[PROJECT_VISION.md](/usnvmemu/docs/PROJECT_VISION.md)** — 项目愿景: 用户态 NVMe firmware 为核心 + 3 transport (OpenHCL/OpenVMM/QEMU vfio-user) + NVMe-oF TCP
> - [ROADMAP.md](/usnvmemu/docs/ROADMAP.md) — 短/中/长期 phase 列表 (动态)，按 Tier 1/2/3 优先级排
> - [PRINCIPLES.md](/usnvmemu/docs/PRINCIPLES.md) — 不变约束 + coding policy + subagent reviewer prompt 模板 + 测试命名约定
> - [LESSONS.md](/usnvmemu/docs/LESSONS.md) — 18 条踩坑教训 (含 decision-then-IO 借用模式 / WebFetch 工作流 / 手算 offset 速查表 / doc audit 必配 git log)
> - [DECISIONS.md](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) — 本 crate ADR (003-008)；跨切面见 [项目 DECISIONS](/usnvmemu/docs/DECISIONS.md)
> - [tls-psk-survey.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)
>   — rustls external-PSK 调研 + 决策
> - [extract-from-openvmm-survey.md](/usnvmemu/docs/2026-06-06-phase-x-extract-from-openvmm-survey.md)
>   — 外部化调研 + 决策推迟 (ADR-008/009)

## Status (V-series 冻结于 V-interop-8；下方明细 2026-06-06 截面)

> 全貌与最新真相见 [NVME_OF_TCP.md §7](/usnvmemu/docs/NVME_OF_TCP.md) + [ROADMAP §0](/usnvmemu/docs/ROADMAP.md)（2026-06-13 更新）。

**所有 phase 代码 + 测试 shipped**: 306 lib + integration tests pass，clippy 0 warning（冻结点 commit `714029df1`）。

**已 verified 在真 Linux nvme-cli**: plaintext `discover` + `connect` + IO (kernel 6.6.114 nvme-tcp.ko, WSL2)。**2026-06-14 真 WS2025 Windows inbox initiator 也实测出盘**（`stornvmeofi`/`nvmeofutil`，static controller model + transport-SGL Connect via R2T，见下表 + [MILESTONES §4.14](/usnvmemu/docs/MILESTONES.md)）。**TLS / mTLS / DH-HMAC-CHAP 等安全栈未走真 host 实测**，只走 lib test + Python harness (跨进程但同一份 Rust 算法对自家 Python 算法)。

| Phase 组 | 状态 | 关键 commit |
|---------|------|------------|
| V0..V5 (PDU / framing / Fabric / Identify / IO Read+Write 闭环) | ✅ shipped | `fc0d43b0` (V5d) |
| V5e1/2 (IO nlb 1→16) | ✅ shipped | `1ba2cd5e` |
| V6 (AER delivery) | ✅ shipped | `c6e03aa6`..`d16f6ca0` |
| V7 (Discovery Log Page 0x70) | ✅ shipped | `3f3b79b1`..`dcfec8ed` |
| V8a/b/c/d/f (multi-conn / Disconnect / dual-listener) | ✅ shipped | `d801306c`..`5a009d48` |
| V8e tokio refactor (KATO / AER Notify / 真并发) | ✅ shipped | `06a16176`..`eb04fcf4` |
| V-followup-tls/mtls (TLS 1.3 + mTLS + NQN<->cert binding) | ✅ shipped | `0534be2a`..`ee820358` |
| V-followup-auth (host NQN allowlist) | ✅ shipped | `1646ac35` |
| V-followup-dhchap-1/2/3/3-wire (simplified HMAC-only CHAP) | ✅ shipped | (V-followup 段) |
| V-followup-dhchap-4 + 4d (spec § 8.13.5 4-msg wire + multi-descriptor) | ✅ shipped | `eff95619` + `714029df` |
| V-followup-prp-list (session-level chunking 16→256 LBA) | ✅ shipped | `2a4d734b` + `619f8d44` |
| V-followup-tls-psk (TP-8011 deterministic crypto) | ✅ shipped | `43040427` |
| V-interop-1..8 (真 Linux nvme-cli + Python harness) | ✅ shipped | 多 commit |
| Linux nvme-cli plaintext discover + connect + IO 真互通 | ✅ verified | [LESSONS](/usnvmemu/docs/LESSONS.md) §14 |
| **真 WS2025 Windows inbox interop (static model + transport-SGL Connect via R2T)** | ✅ verified (真机出盘) | C1 `e2c022575` / C2 `9f1ae4d87` / C3 `4df5c3f83` |

下一步 HIGH 优先 (见 [ROADMAP §1](/usnvmemu/docs/ROADMAP.md)):
- real-host CHAP interop (跑真 Linux nvme-cli `--dhchap-secret`)
- kernel-CI 五元组 anchor for `src/tls_psk.rs`
- TLS PSK wire 注入 (等 rustls upstream external-PSK API)

## ⛔ Security warning

**默认 plaintext / 无 in-band auth / 无 host NQN 白名单**。可通过 CLI flag
启用：
- `--tls-listen` + `--tls-cert/--tls-key` → TLS 1.3 server-auth (V-followup-tls)
- `+ --tls-client-ca` → mTLS 强制 client cert (V-followup-mtls)
- `+ --tls-bind-nqn-to-cert` → NQN ↔ cert SAN/CN 绑定 (V-followup-auth-2)
- `--allow-host-nqn <nqn>` → host NQN 白名单 (V-followup-auth)
- `--host-secret <nqn>=<hex>` → DH-HMAC-CHAP (V-followup-dhchap)

未配置 TLS / auth 时：
- 默认 `--listen 127.0.0.1:4420` 只接受同机 connection
- 非 loopback 地址需显式 `--i-know-this-is-insecure` 才启动 + WARN
- 生产 / 共享 LAN 部署务必加 IP 层 ACL / WireGuard / Tailscale 隧道

> **教学/生产边界**：DH-HMAC-CHAP HMAC-only (无 DH ephemeral)；TLS PSK 仅
> deterministic crypto，rustls 注入待上游 (详 [tls-psk-survey](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md))。
> 各 section 都标"教学版简化"或"教学/生产边界"段。

## Build & Run

```bash
cd usnvmemu/crates/nvme_of_tcp_target
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

> **2026-06-06 update**: V8e-4 已加 `aen_notify: Arc<Notify>` 让 inject_aen
> 触发 < 10 ms 内 wake AsyncSession select；V8c 加 per-conn AER routing。
> 真 SMART threshold timer 未实施 (低优先级)。手动测：见上面 `inject_aen`
> 编程接口段。

## 当前已知限制 (剩余) — 2026-06-06 audit

> 原"V5 已知限制 V6+ 解除"表绝大多数已 ✅ 解除；以下是 *现状* 残留。

| 限制 | 原因 | 解除路径 |
|---|---|---|
| 单 IO ≤ 128 KiB (256 LBA) | session-level chunking (V-followup-prp-list) 透明 16→256 LBA；> 256 LBA SC=0x18 | future controller PRP-list path (见 [DECISIONS](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) ADR-006) |
| TLS PSK 不能注入握手 | rustls 0.23 无 external-PSK API | 等 rustls upstream，见 [tls-psk-survey](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md) |
| DH-HMAC-CHAP HMAC-only | 教学版无 DH ephemeral key exchange | V-spec-strict-mode (见 [ROADMAP §3](/usnvmemu/docs/ROADMAP.md)) |
| `tls_psk.rs` self-consistent 测试 only | 缺 Linux kernel 真五元组 anchor | kernel probe module dump (ROADMAP §1 HIGH) |
| `parse_negotiate` 多 protocol descriptor 已支持 (V-dhchap-4d) | — | — ✅ |
| 单 backing file 真 multi-conn 已支持 (V8b `Arc<SharedControllerInner>`) | — | — ✅ |
| Discovery subsystem 已支持 (V7) | — | — ✅ |
| TLS + DH-HMAC-CHAP 已支持 (V-followup-tls / V-followup-dhchap-3+4) | — | — ✅ |
| 单 IO queue per session 已扩到多 IO queue (V8 + V8e) | — | — ✅ |
| 已 Ctrl-C / SIGTERM graceful shutdown | tokio::sync::watch shutdown | — ✅ |
| ~~大块 IO 性能差~~ | V8e tokio + 透明 chunking 大幅改善；fio 实测见 V-interop-7 load_test.py | — ✅ |

> 下方按 phase 分组的"## V6 AER" / "## V8e tokio" / "## TLS 教学开关" 等
> 章节是 **enable / 使用文档**，不是 status；详细 status 见上表。

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
cd ../nvme_firmware && cargo test --lib
```

`cargo test -p nvme_of_tcp_target` 当前 **306 lib + integration tests 全 green**（冻结于 V-interop-8）；
上面三条命令是按用途拆分的子集（早期 78 + 1 + 67 截面仅作示意，以 crate 全量 306 为准）。

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

## host NQN 白名单（V-followup-auth）

最轻量的 "拿错 NQN 配置" 防护。可重复 `--allow-host-nqn`：

```bash
nvme_of_tcp_target \
    --listen 127.0.0.1:4420 \
    --backing-file disk.img \
    --allow-host-nqn nqn.2014-08.org.nvmexpress:uuid:host-a \
    --allow-host-nqn nqn.2014-08.org.nvmexpress:uuid:host-b
```

行为：
- 若至少给一个 `--allow-host-nqn`，所有 Fabric Connect 必须出示 set 内的
  `hostnqn` 才能通过；否则返 SC=0x84 `CONNECT_INVALID_HOST` 关连接
- 若未给（默认）行为 100% 同 V8（不限制）
- 与 TLS / mTLS 正交，可叠加（实际生产**必须**叠 mTLS：纯白名单挡不住
  恶意 peer 伪造 hostnqn）

### 教学/生产边界

- ✅ 挡 "host 配错 NQN" 类误用
- ❌ **不是** 身份认证：未叠 mTLS / DH-HMAC-CHAP 时 hostnqn 是明文自报
  （spec § 5.2 Fabrics Connect.HOSTNQN），任何 peer 都可声称自己是任意 NQN
- ❌ 不绑 NQN ↔ TLS cert SAN（生产需要 cert SAN binding；留 V-followup-auth-2）
- ❌ 不做 NQN ↔ DH-HMAC-CHAP shared secret（spec § 8.13.5；留 V-followup-dhchap）

## NQN ↔ TLS cert SAN/CN binding（V-followup-auth-2）

最强 host identity 校验：mTLS handshake 完成后从 client cert leaf 抽出
SAN URI + SAN DNS + Subject CN 作为 host identity set；Connect 时
`hostnqn` 必须 ∈ identity set，否则 SC=0x84。

```bash
nvme_of_tcp_target \
    --listen 127.0.0.1:4420 \
    --tls-listen 127.0.0.1:8009 \
    --tls-cert server.pem --tls-key server.key \
    --tls-i-trust-this-cert \
    --tls-client-ca client-ca.pem \
    --tls-bind-nqn-to-cert \
    --backing-file disk.img
```

行为：
- 仅当 mTLS 路径生效（`--tls-client-ca` 配套必填）
- `--tls-bind-nqn-to-cert` 是 spec § 8.13 推荐的真正 host identity 认证机制
- identity 提取顺序：SAN URI > SAN DNS > Subject CN
- leaf cert 抽取失败 (无 SAN 也无 CN) 时直接关连接（防 hostnqn 自由声称）

### 签 client cert 时把 NQN 放进 SAN URI

```bash
# OpenSSL CSR config 加 SAN URI
cat > client.cnf <<EOF
[req]
distinguished_name = dn
req_extensions = ext
[dn]
CN = host-a
[ext]
subjectAltName = URI:nqn.2014-08.org.nvmexpress:uuid:host-a, DNS:host-a.lab
EOF
openssl req -new -newkey ec:<(openssl ecparam -name prime256v1) \
    -keyout client.key -out client.csr -nodes -config client.cnf
openssl x509 -req -in client.csr -CA client-ca.pem -CAkey client-ca.key \
    -extensions ext -extfile client.cnf \
    -out client.pem -days 30 -CAcreateserial
```

### caveat 清单

- ✅ 真正实现 spec § 8.13 强制的 NQN ↔ TLS identity binding
- ✅ 三 fallback 来源（SAN URI > SAN DNS > CN）覆盖大多 ops 习惯
- ❌ 不做 cert revocation 检查
- ❌ 不做 NQN format 强校验（任何 SAN URI / DNS 都按字面对比）
- ❌ 与 `--allow-host-nqn` 是**且**关系（同设两关都过才放行）；既要白名单也要
  cert binding 时建议把白名单装满"允许的 NQN"，cert binding 再挡假冒

## DH-HMAC-CHAP 教学开关（V-followup-dhchap）

spec § 8.13.5 的 in-band host authentication。当前 phase 已交付：

| 子 phase | 范围 |
|----------|------|
| V-followup-dhchap-1 | HMAC-SHA256 算法构件（challenge / response / verify / secret store） |
| V-followup-dhchap-2 | `ChapStage` / `ChapNegotiation` state machine + AsyncSession 集成 |
| V-followup-dhchap-3 | bin CLI `--host-secret <NQN>=<HEX>` + 多 conn 共享 store |
| V-followup-dhchap-3-wire | AUTH_SEND/RECV PDU dispatch + 完整 e2e + admin/IO cmd gate |

```bash
nvme_of_tcp_target \
    --listen 127.0.0.1:4420 \
    --backing-file disk.img \
    --host-secret nqn.host-a=ab12cd34ef56...    # >= 32B (256-bit) HEX
```

CLI 行为：
- HEX 解码后必须 ≥ 32 字节（256-bit entropy），否则 hard-fail
- 可重复指定多 host
- 任一参数不合法 → 中文 stderr + exit != 0

session 端行为：
- 注册了 secret → 每条 conn handshake 后自动 `enable_chap(store)`
- Connect 完成后初始化 `ChapNegotiation`：
  - 已知 host → `stage = ChallengeNeeded`
  - 未知 host → `stage = Failed`（CHAP 启用就必须可校验）
  - 空 store → `stage = Disabled`（兼容路径）
- **V-followup-dhchap-3-wire**: 已 gate admin / IO 命令：`stage.is_authenticated()`
  否则返 SC=0x83。`Disabled` 视为已通过（兼容）

### 教学版 wire 协议（简化）

完整 spec § 8.13.5 是 4-message 协议（T_REQ / T_RESP / T_SUCC1 / T_FAIL），
含 sub-protocol negotiation 字段。本教学版简化为 2-message：

```
host → target:  AUTH_RECV (fctype=0x06, 无 data)
target → host:  C2HData(challenge 32B) + CapsuleResp SC=0
host → target:  AUTH_SEND (fctype=0x05, data = HMAC-SHA256 response 32B)
target → host:  CapsuleResp SC=0 (通过) / SC=0x83 (失败)
```

`response = HMAC-SHA256(secret, challenge || hostnqn || subnqn)` 防 cross-protocol replay。

后续可演进 (留 V-followup-dhchap-4+ 后续 phase):
- ❌ 完整 spec 4-message wire (T_REQ/T_RESP/T_SUCC1/T_FAIL + sub-protocol negotiation)
- ❌ mutual auth (host 验 target)
- ❌ ephemeral DH 增量 (forward secrecy)

### 教学/生产边界

- ✅ HMAC-SHA256 transcript 含 (challenge || hostnqn || subnqn) 防 cross-replay
- ✅ constant-time verify 防 timing leak
- ✅ OsRng challenge nonce
- ❌ HMAC-only 模式（无 ephemeral DH = 无 forward-secrecy）
- ❌ host->target 单向（无 mutual auth；留 V-followup-dhchap-4）
- ❌ wire 集成未完成（V-followup-dhchap-3-wire）
- 生产建议同时启 TLS (V-followup-tls-3+) 防 plaintext challenge 泄漏

> **2026-06-06 update**: DH-HMAC-CHAP wire 已落地两条：simplified (V-dhchap-3-wire)
> + spec § 8.13.5 4-message (V-dhchap-4 + 4d 多 descriptor)，自动识别。
> 详 [DECISIONS](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) ADR-005。

## 内部参考

**首选** (持续维护、动态更新):
- [ROADMAP](/usnvmemu/docs/ROADMAP.md) — 短/中/长期 phase 列表 + 历史 phase 索引
- [PRINCIPLES](/usnvmemu/docs/PRINCIPLES.md) — coding policy + reviewer prompt 模板
- [LESSONS](/usnvmemu/docs/LESSONS.md) — 18 条踩坑教训
- [DECISIONS](/usnvmemu/crates/nvme_of_tcp_target/docs/DECISIONS.md) — 本 crate ADR (003-007)；跨切面见 [项目 DECISIONS](/usnvmemu/docs/DECISIONS.md)

**wire / 算法 reference** (新字段同步更新):
- wire spec：[`../../specs/2026-06-04-nvme-tcp-wire-reference.md`](/usnvmemu/crates/nvme_of_tcp_target/docs/specs/2026-06-04-nvme-tcp-wire-reference.md)
- TLS PSK 调研：[`../../plans/2026-06-06-phase-v-followup-tls-psk-survey.md`](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)

**历史 phase 计划** (已 SHIPPED，保留作设计记录；status banner 见各文件顶):
- [V (NVMe-oF TCP 总)](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-04-phase-v-nvme-of-tcp.md)
- [V4 detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-05-phase-v4-detailed.md) /
  [V5 detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-05-phase-v5-detailed.md) /
  [V6 detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v6-detailed.md) /
  [V7 short](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v7-short.md) /
  [V8 detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v8-detailed.md)
- [V8e tokio detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v8e-tokio-detailed.md) /
  [V8e-7 dispatch detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v8e-7-dispatch-detailed.md)
- [V-followup-tls detailed](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-detailed.md) /
  [V-followup-prp-list detailed (SUPERSEDED)](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-prp-list-detailed.md)

**Backend / Python harness**:
- backend controller：[`../nvme_firmware/README.md`](/usnvmemu/crates/nvme_firmware/README.md)
- Python interop harness：[`scripts/interop_py/`](/usnvmemu/crates/nvme_of_tcp_target/scripts/interop_py/) (9 个 script，详 `scripts/interop_py/README.md`)
- Integration tests 命名约定：见 [PRINCIPLES §10](/usnvmemu/docs/PRINCIPLES.md)

## 许可

MIT — 与上游 openvmm 仓库一致。
