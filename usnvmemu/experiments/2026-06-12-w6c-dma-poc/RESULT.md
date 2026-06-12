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

---

## ✅ W6c 真机 e2e — DMA 零拷贝数据路径 PROVEN（commit 7cd8bfa4 码 + direct-view 修正）

**日期**：2026-06-12　**VM**：pcie-remote-exp（真 OpenHCL VM，W6c IGVM）

### 真机调试暴露并修正的承重事实：本 VM alias-map ON

实现初版按源码追踪用 `fd_offset = file_starting_offset + base_addr`（= underhill 自身的
aliased offset）。真机 kmsg 证本 VM **alias-map ON**（`enabling alias map
alias_map=0x200000000000`，因 VTL1/Guest-VSM 启用）。早期为稳妥加的"alias-on 即禁用共享"
门控（H-2）于是把 `sharing()` 返 None → **DMA_MAP count=0 → 无 disk**。

**修正（POC-backed）**：`fd_offset` 改用**裸 guest_address（mshv_vtl_low 直接视图
offset=gpa）**，非 underhill 的 aliased offset。依据：guest NVMe driver 在 PRP/SGL 填裸
VTL0 GPA（不知 VTL2 alias map），server 收到的 DMA IOVA 是裸 gpa；mshv_vtl_low 在
offset=gpa 直接映射该 VTL0 物理内存。本目录的 mmap POC 已在**同一 alias-on VM** 上证
offset=gpa 读出真 guest 数据（W5a 亦用裸 gpa）。故 alias on/off 都用直接视图，
IOVA==fd_offset==裸 gpa，无需按 alias 门控（删 H-2 的"alias-on 禁用"，改 do_share =
shareable && no_bitmap_gating）。

### PROVEN（真机证据）

修正后重建 IGVM + boot + push/launch usnvmemu（256MiB backing）：
- usnvmemu：`DMA_MAP added addr=0x0 size=0xF8000000 zero_copy=true` + `addr=0x100000000
  size=0x8000000 zero_copy=true`（**2 段 = memory_layout.ram() 低/高 RAM，MMIO hole 排除**，
  正是 mmap POC 预测的布局；addr 是**裸 gpa 非 alias-tagged**）；**dma_read fail count=0**。
- underhill：`collected 2 shareable guest-RAM region(s) for DMA_MAP` + `reconnected, Live`。
- guest（PSDirect）：枚举 `PCI\VEN_1414&DEV_00A9` → disable/enable 重 init（usnvmemu 在
  guest 早期 probe 时尚未 Live，故先 Error；Live 后 re-init）→ devnode **Status=OK** +
  **NVMe disk「OpenHCL Userspace NVMe v2.0」256MB Online**。
- **真 IO（oracle-1）**：guest 写 4MiB（嵌 marker）+ `Write-VolumeCache` flush + 读回 →
  **markerMatch=True**。usnvmemu 日志 **dma_read OK ×4233 + write/IO ×1255** —— 证 4MiB IO
  真经 usnvmemu 零拷贝 DMA 路径（非仅 NTFS cache）。
- **revive-with-DMA**：usnvmemu 重启后连接器重连 → `reconnected, Live` + **DMA_MAP×2 重发**
  （W6c reconnect 重发 hook 生效，同 C-3）。

**= W6b 里程碑达成：guest 枚举 DEV_00A9 + 驱动设备 + 真 4MiB IO（零拷贝 DMA）。**

### finding-⑤（已 root-cause + 已规避，= 诊断 harness artifact 非产品缺陷）

oracle-2 初版用 VTL2 侧 `ohcldiag-dev run grep -a` 扫 256MB backing file，**可复现触发 VTL2
panic-reboot**（os-error-10053、/proc/uptime 重置、/tmp 清空、kmsg 从 0.0）。关键观察：
触发器是**读大 mmap'd 文件**，**非 IO 路径**（IO 本身干净：4233 reads/1255 writes 无崩）。

**根因（差分确认）**：改用**流式 `ohcldiag-dev file -p /tmp/nvme_backing.img | host-grep`**
在**同一个 256MB 文件**上**成功**（marker 命中 @ byte 22577152，VTL2 uptime 104s 不重置）。
同文件 grep 崩、file-p 通 → 坐实 **finding-⑤ = `grep -a` 在无换行的二进制上把整个 256MB
当"一行"缓冲 → 在 512MB-RAM 的 VTL2 里 OOM → 内核 `oops=panic` → reboot**。这是**诊断手段
用错工具（grep-on-binary）的 harness artifact，不是 W6c 产品缺陷**。

**规避**：从 VTL2 取大文件用**流式 `file -p` 导出到 host 再扫**（host 内存充裕），或 `strings`/
bounded read；**勿在 VTL2 内对二进制 `grep`**。

### ✅ oracle-2 PASS — 独立验证完成

`ohcldiag-dev file -p /tmp/nvme_backing.img | grep -a -bo "$MARKER"` →
**`22577152:USNVMEMU-W6C-DMA-REVERIFY-20260612`** —— marker 在 raw backing file 的字节
偏移 22577152（≈21.5MiB，NTFS 给 4MiB 文件分配的簇位置）被独立读出。**这是独立于 guest
NTFS readback 的第二 oracle（raw 字节，不经 guest 文件系统）**，证 guest 写的数据真持久化到
backing store。VTL2 全程存活（usnvmemu pid 不变、uptime 不重置）。

**W6c 真机 e2e 双 oracle 全通**：oracle-1（guest readback markerMatch）+ oracle-2（独立 raw
backing 扫描 @ offset 22577152）+ usnvmemu 4233 dma_read OK / 1255 writes + DMA_MAP×2
zero-copy + revive 重发。**guest 枚举 DEV_00A9 + 驱动 NVMe 设备 + 真 4MiB 零拷贝 IO，
端到端双重独立验证完成。**

### oracle-2 复跑（用流式 file-p，勿在 VTL2 grep 二进制）
```bash
ohcldiag-dev pcie-remote-exp file -p /tmp/nvme_backing.img | grep -a -bo "<MARKER>" | head -1
```


