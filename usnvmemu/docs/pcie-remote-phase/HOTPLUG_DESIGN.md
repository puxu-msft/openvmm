# K-20: pcie_remote hotplug 设计草案 (v2)

> **✅ SHIPPED (2026-06-06 audit 修正)** — commit `a99cdc63` (`feat(pcie_remote): K-20 hotplug 实施 — listener 永不退 + worker transport refresh`) + `64da8fb6` (持久 PolledSocket 替代每轮 rebind) + `93c5fa5f` (SESSION_LOG + spec K-20 v2 backlog → ✅ v9 完成) 已实施。**前一轮 audit (2026-06-06 上午) 误标 "未实施"，本次修正。** spec §10 K-20 已从 P2·backlog 转 ✅ v9 完成。
> v1 行为：listener 一次性 accept，handshake 完进 prepared_map，listener 退出；
> 设备 Lost 后 terminal，host 重启无法恢复。
>
> v2 (本文档) → v9 实施：listener 永不退 + worker transport refresh，本文档作为
> 实施前的设计记录保留。

## 目标

支持 3 个 hotplug 场景，让 host 不必"先于 VM boot 启动":

1. **Host late-attach**: VM 启动后再起 host noop_host_vsock，guest 看到 absent 设备变 Live
2. **Host restart**: host 进程崩 / 升级 / 重启时 guest 继续运行，host 回来后 device 自动恢复
3. **Guest 长跑 + host 周期重连**: VM 周末/月级长跑期间 host 进程可任意维护

## 当前阻碍（v1 设计选择）

- `handshake_spawn` listener task 在 `prepared.lock().insert(id, prep)` 后立即 return → listener socket drop → 端口释放（vsock）/ socket close（TCP）→ host 后来连不上
- `resolver::assemble_device` 一次性 consume `prepared`：移走 transport → spawn worker；map 中 entry 被 remove。后续 host 再连入无对应 prepared
- `Worker<T>` 持 generic transport `T`，无 mechanism 在 runtime 换新 socket
- `SharedState` enum 只 Connecting/Live/Lost，Lost 是 terminal（A4 ≥4 bad-frame 走的就是这条路）

## v2 设计

### Architecture 变更

```
                  +-----------------+
                  | listener task   |  ← 永远不退出
                  | (per instance)  |    accept loop:
                  +-----------------+    handshake → send transport via channel
                          |
                          | TransportSwap channel
                          v
                  +-----------------+
                  | Worker          |  ← select_biased! 加 transport_refresh arm
                  | (single Task,   |    收到新 transport：
                  |  long-lived)    |      1. drain in_flight (NoResponse)
                  |                 |      2. replace self.transport
                  +-----------------+      3. state.store(Live) (从 Lost 复活)
                          |
                          v
                  +-----------------+
                  | PcieRemoteDevice|  ← state 看到 Live 后恢复 forward MMIO/cfg
                  +-----------------+
```

### 关键代码改动

#### 1. transport 类型擦除（已有：`BoxedTransport`）

worker 持 `BoxedTransport` 而非 `Worker<T> generic`。现 prepared.rs::AsyncTransport
trait 已经定义；只需 worker 端从 `Worker<T>` 改为 `Worker { transport: BoxedTransport, ... }`。

#### 2. listener task 永不退出

```rust
// handshake_spawn.rs (v2)
async fn listen_forever<L>(
    listener: L,
    instance_id: Guid,
    handshake_timeout: Duration,
    transport_swap: mesh::Sender<BoxedTransport>,
    prepared_seed: PreparedMap,   // first handshake 走 prepared map（boot grace）
) {
    let mut first = true;
    loop {
        // accept + handshake 同 v1
        match accept_and_handshake(...).await {
            Some(prep) if first => {
                prepared_seed.lock().insert(instance_id, prep);
                first = false;
                // 不 return；继续循环
            }
            Some(prep) => {
                // 后续 handshake：发新 transport 给 worker（覆盖旧）
                if transport_swap.send(prep.take_transport()).is_err() {
                    // worker dropped — listener 也可退（device 已无消费者）
                    return;
                }
            }
            None => {
                // 单次 handshake 超时；继续 accept 等下一个 host connect。
                // 这是 hotplug 关键 — host 后启即可被发现。
            }
        }
    }
}
```

#### 3. Worker select_biased! 加 transport_refresh arm

```rust
// worker.rs (v2)
pub async fn run(mut self, mut shutdown: Receiver<()>, mut refresh: Receiver<BoxedTransport>) {
    loop {
        select_biased! {
            _ = shutdown.next().fuse() => break,
            new_transport = refresh.next().fuse() => {
                let Some(new) = new_transport else { /* sender drop */ break };
                tracing::info!(CVM_ALLOWED, "pcie_remote: transport refreshed");
                self.drain_in_flight();
                self.transport = new;
                self.state.store(DeviceState::Live);  // 从 Lost 复活
                self.consecutive_bad_frames = 0;
                self.stats.consecutive_bad_frames.store(0, Ordering::Relaxed);
            }
            req = self.from_device.next().fuse() => { ... }
            inbound = codec::read_frame(&mut self.transport).fuse() => {
                match inbound {
                    Ok(m) => { if !self.dispatch_inbound(m).await { ... → 进 Lost, **不 break** } },
                    Err(e) => {
                        // transport 死了 — 但**不 break worker**
                        tracing::warn!(CVM_ALLOWED, error = %e, "transport dead; awaiting refresh");
                        self.state.store(DeviceState::Lost);
                        // 阻塞在下一轮 select_biased! 等 refresh
                        // 注意：read_frame 已 return Err，下一轮 select 会再调 read_frame，
                        // 它会再次失败 → 死循环 burning CPU。需要在 Lost 时跳过 read arm。
                        // 用 'if let Some(_)' guard 或 enum 模式。
                    }
                }
            }
        }
    }
}
```

#### 4. SharedState Lost 不再 terminal

device.rs::pci_cfg_read / mmio_read 在 Lost 时：
- cfg_read: 返回 !0（让 guest 看到 device 'gone'）
- mmio_read: 返回 IoError::NoResponse
- cfg_write/mmio_write: 静默 swallow（guest 继续）

worker.rs::run 转 Live 后，device.rs 自动恢复 forward（state.load() 是动态读 atomic）。

#### 5. 协议层无变更

Hello/HelloAck handshake 仍是 connection-init；新 connection 自然走完整 handshake。
host 端 noop_host vsock_main 已经有 outer reconnect loop（v3 加的），无需改。

### 风险 + 边界

| 风险 | 缓解 |
|---|---|
| 旧 host 还活着 + 新 host 同时连入 | listener accept FIFO，新 transport 替换旧；旧 transport drop → 旧 host EOF → 自然退出 |
| 新 host 的 DeviceDescribe 与旧不同（不同 vendor/device/BAR） | v2 验证：refresh 时丢弃新 describe，强制与旧一致；不同则拒绝 + 关旧连接（device 进 Lost 等下一次） |
| guest 在 Lost 期间 fire MMIO read | device.rs::mmio_read 返回 NoResponse；guest driver 看到 IO error；driver 自行决定 retry / disable device |
| in_flight 满（大量 pending MMIO read） + Lost | drain_in_flight 已实现；refresh 时也 drain |
| Worker CPU 100% busy-loop on dead transport | **必须**在 Lost 时 select arm 跳过 read_frame。用 `state.load() != Lost` guard |

### 测试矩阵

| 场景 | 预期 |
|---|---|
| Host 先 noop，VM 后启 (v1 行为) | OK，同 v1 |
| VM 先启 (handshake timeout) → device Absent → host 后启 | hotplug ✅: listener 仍 listen，host connect 触发 handshake，prepared insert → device 装配 → Live |
| host 进程 kill -9 → guest 继续运行 → host restart | hotplug ✅: 旧 transport EOF → worker.state=Lost → refresh channel 等待 → 新 host connect → listener send 新 transport → worker Live |
| host 重启时 guest 有 inflight MMIO | drain_in_flight NoResponse → guest driver 看到 error → driver 自处理 |
| host 重启时 guest 触发新 MMIO（Lost 期间） | NoResponse 立即返回 |

### 工程量估计

- listener_forever: 30 LOC
- worker refresh arm + state Lost-recovery: 80 LOC
- device.rs Lost 期 NoResponse 行为已有：5 LOC 调整
- 测试覆盖: 200 LOC（refresh / restart / inflight drain）
- e2e 在真 Hyper-V 验证: 0.5 天

### 决定

**v2 实施，不在当前 v1 范围**。spec §10 K-20 已标 v2 P0；本 doc 是开 PR 时
的"实施蓝图"，让 reviewer 看到完整 plan，知道 v1 的"Lost terminal" 是
**深思熟虑的简化**而非疏忽。

### 引用

- v1 实测验证：[../PCIE_REMOTE_SESSION_LOG.md](SESSION_LOG.md) `--stress-bad-frames` 段
- v1 worker.rs::run: [../../../vm/devices/pcie_remote_device/src/worker.rs](/vm/devices/pcie_remote_device/src/worker.rs)
- BoxedTransport: [../../../vm/devices/pcie_remote_device/src/prepared.rs](/vm/devices/pcie_remote_device/src/prepared.rs)
