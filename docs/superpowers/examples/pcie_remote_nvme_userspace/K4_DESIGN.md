# Phase K4 — Real PI interleaved IO (deferred to micro-Phase K4b)

## 概述

Phase K1 已铺设 SECTOR_SIZE per-NS + T10 DIF CRC16 引擎（src/pi.rs）+
Format NVM 真切换 LBAF[1]+PI Type 1。**剩下的 K4** 是让 5 个 IO 路径
(READ / WRITE / COMPARE / WRITE ZEROES / VERIFY) 都按
`ns.lbads/meta_size/pi_type` 真做 interleaved data+meta IO + CRC
verify/compute。

## 实现量评估

每个 IO 路径 (READ/WRITE) 当前结构：
- ≤ 1 page：单 PRP DMA
- ≤ 2 page：双 PRP DMA（WriteAccum）
- > 2 page：PRP list 多页（PrpListOp）

每档 × 5 个 opcode × {data-only, data+PI} = 30 个变体路径需要分流。

具体改动量：
1. **Write 路径**：
   - DMA-read 完成回调中按 ns.pi_enabled() 走 PI 分支
   - PI 分支：把 driver 提供的 4 KiB 数据切成 4096-byte chunks
     (每 chunk = 1 LBA 数据)，对每 chunk 计算 PiTuple
     (Guard CRC16 + RefTag = lba & 0xFFFFFFFF)
   - 写到 backing file 时按 ns.block_bytes() (= 4096 + 8) interleave：
     data[4096] + tuple[8]，offset = lba * 4104
2. **Read 路径**：
   - 读 backing file `nlb * 4104` 字节（含 PI tuples）
   - 拆开：对每 LBA 提取 4096 data + 8 tuple
   - verify PI tuple：guard CRC 与 data 重算一致 + RefTag 与 LBA 一致
   - 任何 LBA verify fail → 整个 Read 返 SC=0x82 GUARD_CHECK_ERR
     / 0x84 REF_TAG_CHECK_ERR (SCT=0x02 Media/Data Integrity)
   - 成功：只 dma_write data 部分到 PRP1（4 KiB per LBA）
3. **Compare**：与 Read 同样需 PI verify + 然后才与 host 数据 byte 比较
4. **Write Zeroes**：写 LBA 时 controller 自己 compute zero-data 的
   PI tuple（CRC16(0..0) = 0；RefTag = lba）
5. **Verify**：只读 backing + verify PI；不传 data

## 设计选择

### 选项 A — 真 interleaved file

正确 spec 行为：file 大小 = total_lba × (4096 + 8)。
- 优点：与真硬件 metadata-in-band 模型对齐；driver 实现纯
- 缺点：file size 不再是 power of 2；现有 backing 文件需 reformat

### 选项 B — Separate data + metadata files

backing/ns1.data 存纯数据，backing/ns1.meta 存 PI tuples (8B/LBA)。
- 优点：data file 可被 host filesystem 当普通块设备访问
- 缺点：2 个 file handle 增加管理；与 driver 看到的"single namespace"
  语义不太一致

**选 A**，与 NVMe spec § 5.17.2 metadata layout 一致 (LBAF.MS = 8
说的就是 inline metadata)。

## 单 LBA prototype（已搭好 hook 然后回退）

我之前临时在 mod.rs 的 PendingOp 里加了 `NvmWritePiSingleLba { lba }`
变体准备走完整 Write 单 LBA PI 路径，但发现：
- IO 路径分流需要全部 5 个 opcode + 全部 PRP 分档同步改造
- 每个完成路径都需要 PI compute / verify 接入
- 加上 sibling cleanup / op_id 跟踪
- 实施量约 800-1500 行新代码 + 5-10 个新 unit test

回退原因：教学价值 OK 但代码量太大，应作为独立 commit 系列 K4a/K4b/K4c
分别覆盖 Write / Read / Compare/WZ/Verify。

## 留 future 真做时的步骤

1. K4a Write 路径：
   - dispatch_io WRITE：检 ns.pi_enabled()，PI 路径用新 PendingOp
     `NvmWritePi { lba, num_blocks, pi_pad }`
   - 完成回调按 LBA 切 data，compute PiTuple, interleave 写文件
2. K4b Read 路径：
   - dispatch_io READ：PI 路径读 file `nlb*4104` 字节，拆 + verify
   - 失败用 PiCheck::to_sc() 返 SC
   - 成功 dma_write 仅 data 部分
3. K4c Compare/WZ/Verify：
   - Compare：先 PI verify backing → 然后与 DMA-read host data 比较
     （兼容 host data 不含 PI tuple，spec § 3.3.2 'compare with host data
     only'）
   - Write Zeroes：自 compute zero data 的 PI tuple 写入
   - Verify：仅 PI verify，无 data transfer
4. 单测：crc16_known_vector + per-LBA round-trip Write/Read + verify fail

## 当前替代方案

K1 已声明 DPC capability + 让 Format 真切换 ns.pi_type，但 IO 路径
guard 上写：'pi_enabled NS → INVALID_FIELD'。Driver 看到能 Format
但 IO 不能用 PI → 倒退去 LBAF[0] PI=0 默认。**对教学完整性是
opportunity loss，对正确性是安全**（不会数据损坏）。
