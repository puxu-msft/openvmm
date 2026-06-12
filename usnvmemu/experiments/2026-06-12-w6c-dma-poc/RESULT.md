# W6c POC：高 GPA 跨进程 mmap /dev/mshv_vtl_low 验证（策略 A 承重假设）

**日期**：2026-06-12　**VM**：pcie-remote-exp（真 OpenHCL VM，运行 finding-③ 修订后的 vpci IGVM，设备 Live）

## 目的

finding-④ 的修复选了**策略 A**：给 underhill 的 VTL0 `GuestMemory` 实现
`GuestMemoryAccess::sharing()`，让设备把每个 guest RAM 段经 vfio-user DMA_MAP（SCM_RIGHTS
传 `/dev/mshv_vtl_low` fd）映射给 VTL2 里的 usnvmemu 零拷贝 DMA。

源码追踪（subagent）已确认策略 A 在**非隔离 VTL0** 路径安全（`build_without_bitmap` →
`valid_memory`/`permission_bitmaps` 均 None → `access_bitmap()` 返 None → 无 bitmap 门控；
backing 由 hypervisor commit + `MAP_SHARED` 同 fd），并给出 `file_offset = guest_address +
vtl0_alias_map_bit.unwrap_or(0)` 的公式。但它明确标出**只能靠真机定论的残留假设 #1**：
高 GPA 的跨进程 mmap 能否返回真 guest 内存——W5a 只在低 GPA 0x100000 证过机制，没测高 GPA。

**这条假设不靠推理、靠 POC 钉死**（[[poc-before-settling-design]] 教训：源码确认线性 ≠
真机确认；显然该成立的最该验）。

## 做法

写了个 40 行独立 musl 工具 `peek/`（open `/dev/mshv_vtl_low` → mmap 到指定 GPA →
dump 64 字节 + 解释 NVMe SQE）。经 base64-stdin 推进 VTL2 跑，打 usnvmemu 当时正 DMA-fail
的那个 admin SQ GPA（0xeae2b000）+ 一组高/低 GPA 对照。

## 结果

| GPA | 结果 | 含义 |
|-----|------|------|
| **0x100000**（低，W5a 对照） | `68 62 69 6e`=`"hbin"` + UTF-16 `"NC_AddRemoveCo…"` | 跨进程 mmap 返回**活 guest RAM**（Windows 注册表 hive）✓ |
| **0xeae00000**（高，3.67 GB） | `opcode=0x88 prp1=0xffff8107eed1e1c8` 非零非 FF | **高 GPA 返回活 guest 内核 RAM**（`0xffff8…` 经典 Windows 内核指针）——**承重假设 #1 PROVEN** ✓ |
| **0xeae2b000**（DMA-fail 的 admin SQ） | 全零（无 SIGBUS） | 该 RAM 页 backed 就绪；零是因 stornvme 超时已释放该队列（残留 #3 SIGBUS 解决）✓ |
| 0x80000000 / 0xc0000000 / 0xe0000000 / 0xf0000000 | 全 FF | guest 物理空间的 MMIO hole / 未 backing 段 |

## 结论（三个，全是真机定的，非推理）

1. **承重假设 #1 PROVEN**：高 GPA（3.67 GB）的跨进程 mmap 返回真 guest RAM，linear
   `file_offset = guest_address` 在高地址成立（0xeae00000 读对即证；它 > 0xe0000000 但只有
   它有数据，说明差异是布局/洞，不是"高地址 offset 失效"）。
2. **alias-map OFF（本 VM）**：peek 用 `fd_offset = guest_address`（未加任何 alias bit）就
   读对低/高两处 → 本 VM `vtl0_alias_map_bit = None`，残留假设 #4 对本 VM 解决；实现里
   `fd_offset = guest_address`（但代码仍应从 builder 的 `physical_address_base` 推导以兼容
   alias-on 配置，见源码追踪 Q2）。
3. **必须逐 `memory_layout.ram()` 段 map**：高地址区有洞（多处 FF）→ 一整块 map 会覆盖到
   非-RAM 洞。直接验证了策略 A 设计"每个 RAM 段一个 DMA_MAP / ShareableRegion"的正确性，
   证伪了"一整块 map"的偷懒做法。无 SIGBUS 也说明 RAM 页开机即 backed（残留 #3）。

## 残留（仍未完全定，留意但不阻塞）

- 残留 #2（underhill 的 `for_kernel_access(true)` MemoryRegistrar 与外部进程 mmap 共存）：
  peek 进程与 underhill 并存 mmap 同 fd 未见冲突（弱证据）；真正验证是 usnvmemu 在 DMA 期
  并存，由后续 disk+IO 真机复验覆盖。
- 多 RAM 段 → 每次 reconnect 多次 DMA_MAP 往返（性能特征，非正确性）。

## 复跑

```bash
cd peek && cargo build --release --target x86_64-unknown-linux-musl
base64 -w0 < target/.../w6c_peek | ohcldiag-dev pcie-remote-exp run -- sh -c 'base64 -d > /tmp/w6c_peek && chmod +x /tmp/w6c_peek'
ohcldiag-dev pcie-remote-exp run -- /tmp/w6c_peek 0xeae00000 64
```
