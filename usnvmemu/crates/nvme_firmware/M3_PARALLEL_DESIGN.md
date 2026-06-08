# Phase M3 — Per-queue Parallel Dispatch (架构决策记录)

## 当前模型（单线程串行）

SDK 的 `run` 主循环 (在 `pcie_device_sdk/src/run.rs`) 是
**单线程 async loop**：
1. select transport (vsock/tcp) read 或 timer tick
2. 收到 `ToHost` message → `dispatch_inbound` 同步派发到 `PcieDevice` trait
3. `PcieDevice::mmio_write` / `on_dma_complete` 都拿 `&mut self`

NVMe controller (`NvmeController`) 实现 `PcieDevice`，单线程地：
- 收 MMIO doorbell 写 → `dispatch_sqe` enqueue SQE
- `on_dma_complete` 收 SDK 异步 DMA 完成 → 推进 IO state machine

**多 queue 并发只体现在 wire 层**：4 个 SQ 各自的 fetch DMA-read 可以在
vsock 上 in-flight 并发；但 host 端 dispatch 是串行。

## 为什么不真做 per-thread dispatch

要让 4 个 IO queue 真正在 host 上并行，需要：

### 1. SDK 改造（高代价）

`PcieDevice::on_dma_complete(&mut self, ...)` 是 single-mutable-borrow，
SDK 主循环一次只 dispatch 一个。要真并发：
- 改成 `Arc<Mutex<dyn PcieDevice>>` → 每个 callback 抢 lock，对教学清晰度
  减分（锁竞争 + deadlock 风险）
- 或拆 PcieDevice trait 成 per-queue actor：每队列独立 worker，跨队列通信
  走 channel。这是真正的 multi-actor 模型，但需要重新发明 inter-actor 通信
  协议（NS state、Identify cache、SMART counter 都得 cross-actor）

### 2. Backing file IO 真并发

每 `Namespace` 单 `File` handle。多线程同时 `read_at`/`write_at` 在
**positional IO** (pread/pwrite) 下其实是 OS 级原子安全的（Phase H3 修复后
路径已迁移）。所以 file IO 已经具备 thread-safe 基础。

### 3. mmap 视图

`memmap2::MmapMut` 自身是 `&mut [u8]` slice，多线程读 (`&[u8]`) 安全；
并发写需要 split into disjoint slices 或 unsafe pointer split。教学示例
不推荐。

## 当前并发实际表现

测试场景（4 IO queue × 16 outstanding cmds/queue = 64 in-flight）：

| 路径 | 并发度 | 说明 |
|---|---|---|
| Wire (vsock DMA) | 4-way | SDK 单线程但 SDK 的 `dma_read/write` 异步入 outbound queue，wire 多个并发 |
| Host dispatch | 1-way | 单 thread `on_dma_complete` 串行处理 |
| Backing file IO | 1-way (per-NS) | `read_at`/`write_at` 在 single thread 调用串行 |
| Page cache | OS 级并发 | mmap 让 page cache 命中跨 IO 无 host CPU 开销 |

实测 throughput 上限约 5000-10000 IOPS 单 NS（vsock RTT 主导，~50-200 μs
per round trip）。真多线程能把它推到 30-50k IOPS（host CPU 解放），但对
教学性 NVMe demo 没有质的差别。

## 解禁路径（future work）

如果某天真做：
1. 把 SDK 改成 spawn N worker actor，每个 actor 单 SQ；inter-actor 通信
   走 `crossbeam_channel`
2. NvmeController 拆 per-queue 状态（pending_ios / pending_fetches）
   + shared 状态（Identify / NS state / SMART counter）放 `Arc<Mutex>`
3. backing file 改 `Arc<File>` 让多 worker 共享，positional IO 已 OK
4. Benchmark 对比 single-thread vs 4-thread，文档化性能差异

## 决策结论

Phase M3 **不实现真多线程**。当前 single-thread async model 是合理 trade-off：
- 教学清晰度 (CRITICAL) — 单 thread 让 reviewer 追代码路径线性
- 正确性 (HIGH) — 无 lock / race 隐患（reviewer 8 轮验证 0 critical 也部分
  归因于此）
- 性能 (LOW for teaching) — 5-10k IOPS 已远超 demo 需求

Phase M3 的"价值"全在文档化这个决策、记录改造步骤、保留 future option。

若有 production 需求需要 30k+ IOPS：建议直接 fork 一个 multi-thread
variant，而不是在教学版上加 #[cfg(feature = "multi_thread")] 让代码双
路径化。
