# Phase M2 — Zero-copy backing file mmap ✅ 已完成

## 目标

当前 Read/Write 路径用 `file.seek + file.read_exact / write_all`，每次
IO 都经历：
1. Guest write → vsock → host DMA-read buffer (alloc Vec<u8>)
2. Host write file → kernel copy buf → page cache → fs flush
3. Read：file read → kernel copy page cache → buf → vsock → guest

**4 次 memcpy 路径**（每方向 2 次）。Phase M2 把 host file IO 那 2 次
memcpy 抹掉（fast path），只剩 vsock 那 2 次（需 SDK 改造才能消，留 N2+）。

## 实现

依赖：`memmap2 = "0.9"`（Cargo.toml；事实标准 mmap wrapper，跨 Unix/Windows）。

### 数据结构

```rust
pub(super) struct Namespace {
    pub(super) file: File,
    pub(super) mmap: Option<memmap2::MmapMut>, // 整文件映射
    ...
}
```

Namespace::open 时 `try_mmap_file(&file)` 尝试 mmap；失败（如某些 FS
不支持 mmap）退到 file IO，不阻断启动。

### Hot path

- `Namespace::read_at(buf, off)` — mmap 在时 `buf.copy_from_slice(&mmap[off..off+buf.len()])`
- `Namespace::write_at(buf, off)` — mmap 在时 `mmap[off..].copy_from_slice(buf)`
- `Namespace::flush()` — mmap 在时 `mmap.flush()` (msync/FlushViewOfFile)；否则 `file.sync_all()`

### Wired-in 路径

所有 NVM IO 大热点：
- NvmWriteDmaRead completion (mod.rs)
- NvmWritePi completion
- NvmZoneAppend completion
- dual-PRP Write completion
- PRP-list Write completion
- compare_finalize (Read backing)
- Compare single-PRP completion
- NVM_READ dispatch (io.rs)
- PI READ dispatch
- WRITE_ZEROES (chunked，仍 chunk 以兼容 fallback)

FLUSH NVM 命令 (io.rs:845) 改 `ns.flush()` — mmap 路径下走 msync。

## 测试

`controller::tests::mmap_zero_copy_round_trip`：
- 确认 open() 后 mmap 已 init (Some)
- write_at(4 KiB pattern) → 不 flush → read_at 立即拿回相同 pattern
- 跨 LBA 边界写
- flush() 不 panic

29 tests total passing。

## 安全性

`try_mmap_file` 内部 `unsafe { MmapMut::map_mut(&file) }` 唯一一处 unsafe，
带 SAFETY 注释：教学 controller 单线程独占 backing file，无并发 writer。

## 未做（仍为 perf 上限）

- vsock zero-copy host→guest：需 protocol 改造（共享内存 / sendfile-like
  primitive）。当前 protobuf bytes 字段仍有 2 次 vsock 序列化拷贝。
- NS Resize 时重建 mmap：当前不支持 resize，未来加 NS Mgmt Resize 需
  drop+rebuild mmap。
