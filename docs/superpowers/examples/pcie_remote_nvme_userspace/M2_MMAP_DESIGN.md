# Phase M2 — Zero-copy backing file mmap (deferred)

## 目标

当前 Read/Write 路径用 `file.seek + file.read_exact / write_all`，每次
IO 都经历：
1. Guest write → vsock → host DMA-read buffer (alloc Vec<u8>)
2. Host write file → kernel copy buf → page cache → fs flush
3. Read：file read → kernel copy page cache → buf → vsock → guest

**4 次 memcpy 路径**（每方向 2 次）。

## 真零拷贝设计

用 `memmap2` crate 把 backing file 整体 mmap 进进程地址空间：

```rust
use memmap2::MmapMut;

pub(super) struct Namespace {
    pub(super) file: File,
    pub(super) mmap: MmapMut, // 整文件映射，长度 = file_size
    ...
}
```

Write 路径：
- DMA-read host buf → 直接 `mmap[lba * sector..].copy_from_slice(&buf)`
- 不需 file.write_all（写入直接落在 mmap 上）
- 持久化通过 `mmap.flush_range()` 或 FLUSH cmd 触发 `mmap.flush()`

Read 路径：
- 读 mmap[lba * sector..lba * sector + bytes] → DMA-write 到 guest

省 2 次 kernel copy；但仍有 1 次 host→guest DMA serialization (protobuf
bytes 字段)。要做真 zero-copy host→guest 需 SDK 协议改造（vsock chunked
zerocopy 或共享内存）。

## 阻塞 Phase M2 真做的原因

1. **依赖管理**：memmap2 是外部 crate，需 OpenVMM workspace 引入
2. **MMU 一致性**：Windows mmap (CreateFileMapping) 与 file.write_all
   并存时一致性需小心；mixing 易出 spec-undefined 行为
3. **错误处理**：mmap 失败回退到 file IO 需双路径维护
4. **测试代价**：现有 IO unit test 已覆盖正确性；mmap 改造主要是性能
   优化，对教学价值递减

## 真要做时的步骤

1. 加 memmap2 deps 到 Cargo.toml
2. Namespace::open 后 mmap_mut 整文件
3. Write 路径 dma_read 完成回调改 mmap slice copy_from_slice
4. Read 路径用 mmap slice → ctx.dma_write
5. FLUSH 改 mmap.flush()
6. 单测：写后 mmap 内容立即可读 (no fsync needed)
7. 真 perf benchmark 对比

## 当前实现的性能边界

- vsock RTT ~50-200 μs per DMA RTT
- file IO ~10-30 μs per 4 KiB (page cache hit)
- 单 IO 至少 2 RTT (PRP fetch + data transfer)
- 实测 ~5000-10000 IOPS 单 queue（vsock 主导）
- mmap 节省 file IO 部分 → 理论 +10-20% IOPS

性能性价比合理时再做（user 原话："不舍性能除非性能性价比过犹不及"）。
当前模型足够展示 NVMe 协议教学完整性。
