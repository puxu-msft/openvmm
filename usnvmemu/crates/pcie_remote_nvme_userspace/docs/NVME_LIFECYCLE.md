# NVMe Lifecycle 教程：从 driver init 到完整 IO

这是 [pcie_remote_nvme_userspace](../) example 的配套教学文档。把 NVMe
2.0 spec 抽象的 controller lifecycle 落到具体代码行号，让读者按"driver
看到 controller 一次完整启动 + 用 + 关闭"的时间线对照 spec 阅读。

---

## 时间线总览

```
1. PCIe enumeration                      [pcie_remote_device → guest BDF]
2. Identify Controller                   [admin opc 0x06 CNS 0x01]
3. Identify Active NSID list             [CNS 0x02]
4. Identify Namespace                    [CNS 0x00]
5. Set Features 0x07 NumberOfQueues      [协商 IO queue 数]
6. Create IO CQ (admin opc 0x05)        [driver 给 N 个 CQ 分配 GPA]
7. Create IO SQ (admin opc 0x01)        [driver 给 N 个 SQ 分配 GPA]
8. Get Log Page 0x02 SMART (可选)        [driver 初始 health check]
9. Async Event Request × 4               [driver 预投 AER 准备接事件]
10. 进入 IO 阶段：Read/Write/Flush…
11. driver 发 Set Features / Get Log Page 动态查 controller 状态
12. Shutdown：CC.EN 1→0 → CSTS.RDY=0     [清 queues + drain]
```

---

## 第 1 步 — PCIe enumeration

OpenHCL 内 `pcie_remote_device` 把我们用户态 controller 当作 emulated
PCIe BDF 暴露给 guest。Guest BIOS / UEFI / Windows PnP 扫到：

```
Vendor ID: 0x1414  (Microsoft，与 OpenHCL pcie_remote 路由匹配)
Device ID: 0x0001  (任意)
Class Code: 0x01_08_02 (Mass Storage, NVMe)
BAR0: MMIO 64-bit, 8 KiB
MSI-X: 4 vectors
```

代码：[`src/cmd.rs:NvmeController::describe()`](../src/cmd.rs) (PCIe DeviceDescribe)
+ [`src/regs.rs`](../src/regs.rs) (BAR0 layout)。

## 第 2 步 — Identify Controller (Admin)

Windows nvme.sys driver 启动时先发 `Identify CNS=0x01`：driver 提供 4 KiB
PRP1 buffer，controller DMA-write 200+ 字段的 IdentifyController struct
到该 buffer。

**关键字段** ([cmd.rs build_v2_bytes](../src/cmd.rs))：
- `VID/SSVID` (0x1414/0)
- `SN` "PCIE-REMOTE-USRSPACE" (20 ASCII，spec 要 left-justified + space pad)
- `MN` "OpenHCL Userspace NVMe v2.0" (40 ASCII)
- `FR` "v2.0    " (8 ASCII firmware revision)
- `MDTS` = 5 → 单 cmd 最大 transfer = 2^5 page = 128 KiB
- `OACS` = Format + FW + Self-Test + NS Mgmt + Doorbell Buf + Directives + GetLBA + Security
- `ONCS` = Compare + Write Zeroes + DSM + Verify + Reservations + Save/Select
- `VWC` = 0x01 (driver 主动发 FLUSH)
- `SANICAP` = 0x07 (Sanitize Block + Crypto + Overwrite)
- `NPSS` = 7 (8 power states)
- `NN` = 注册的 namespace 数

完成路径：
1. driver 写 SQ0TDBL (admin SQ tail doorbell) → MMIO write @ BAR0+0x1000
2. controller 检 doorbell → DMA-read SQE entries 到 host
3. dispatch_admin (admin.rs) 识别 IDENTIFY → build buf → DMA-write 4 KiB 到 driver PRP1
4. controller 写 CQE 到 admin CQ + fire MSI-X vector 0
5. driver 读 CQ → IDENTIFY 完成

## 第 3-4 步 — Identify NSID list + Namespace

- CNS=0x02 返 4 KiB 全 NSID 列表 (u32 LE 数组，0 终止)
- CNS=0x00 NSID=N 返该 NS 的 IdentifyNamespace 结构 (含 NSZE/NCAP/
  LBAF[0..N]/DPS/RESCAP 等)

driver 从此知道：有几块盘，每块多大，sector 大小，是否支持 PI / reservation。

## 第 5 步 — Set Features 0x07 NumberOfQueues

driver 写 `cdw11 = (NSQR-1) | ((NCQR-1) << 16)` 请求多少 IO queue pair；
controller 在 CQE.cdw0 回 `(NSQA-1) | ((NCQA-1) << 16)` 表示实际授予数。

我们 IO_QUEUE_CAP = 4 → driver 请求 ≥4 都拿 4。

## 第 6-7 步 — Create IO CQ + SQ

driver 给每个 IO queue 分配连续物理内存 + 写 Create IO CQ (0x05) +
Create IO SQ (0x01)：
- CQ：QID + QSIZE + IV (interrupt vector) + IEN (enable)
- SQ：QID + QSIZE + CQID (关联 CQ)

controller 把 (qid → base_gpa) 存进 sqs/cqs HashMap (mod.rs:130)。

## 第 8 步 — 初始 SMART log (可选)

driver 发 Get Log Page LID=0x02 → controller DMA-write 512 byte SMART
log；driver 检 critical_warning bit 决定是否走 fallback。

我们的 SMART log 真追踪 host_read_commands / data_units_* / power_on_hours
等 ([logs.rs:build_smart_health](../src/controller/logs.rs))。

## 第 9 步 — AsyncEventRequest 预投

driver 投 4 条 AER (Identify Controller .aerl + 1) 让 controller 缓冲
事件。我们 aen_pending VecDeque 保存。

```
[tick] self.stat_num_err_log_entries > self.aen_last_err_count
       → fire_aen(type=0x00, log=0x01)
[tick] Self-Test 完成 → fire_aen(type=0x02, info=0x01, log=0x06)
[tick] Sanitize 完成 → fire_aen(type=0x02, info=0x05, log=0x81)
```

## 第 10 步 — IO 阶段

### 一次 Read 命令的完整路径

1. driver 在 IO SQ N 写一条 SQE：opc=0x02 READ, nsid=1, slba=100, nlb=8 (4 KiB)
2. driver 写 SQyTDBL → controller MMIO write 回调
3. [`on_sq_tail_doorbell`](../src/controller/mod.rs) → 算新 entry 数 → DMA-read SQE
4. 完成回调 dispatch_sqe → [`dispatch_io`](../src/controller/io.rs) → opcode 分流
5. READ：seek+read backing file → DMA-write 4 KiB 到 PRP1
6. on_dma_complete (NvmReadDmaWrite) → 构 success CQE → post 到 IO CQ N
7. fire MSI-X vector → driver ISR 读 CQE → 完成

### 一次 Write 命令的完整路径

1. SQE：opc=0x01 WRITE, nsid=1, slba=200, nlb=16 (8 KiB)
2. [dispatch_io WRITE](../src/controller/io.rs) → 3 档 PRP 分流：
   - ≤ 1 page (4 KiB) → 单 PRP1
   - ≤ 2 page (8 KiB) → 双 PRP1+PRP2 (WriteAccum 累积)
   - > 2 page → PRP list (PrpListOp 累积，列页指向所有数据页)
3. DMA-read host buffer → 完成回调 (NvmWriteDmaRead / NvmWriteDualPrp /
   NvmWritePrpList*) → seek+write backing file → CQE

### 关于 FLUSH

driver 用 `FLUSH (opc 0x00)` 拿持久化承诺。我们 [`io.rs FLUSH`](../src/controller/io.rs)
对每个目标 NS 调 file.sync_all()。无 FLUSH 时 Write 走 host page cache
（无 per-IO fsync 是 Phase H5 性能修复）。

## 第 11 步 — 动态 Set/Get Features / Log Page

driver 运行期会：
- Get SMART 周期检温度 / 容量 / 错误数
- Get LBA Status 检 unrecovered LBA
- Set Power Management 节能
- Set Async Event Config / Number of Queues 调整资源
- Set Host Identifier (16 byte HOSTID) 关联 multi-host (Phase K9)

## 第 12 步 — Shutdown

driver 写 CC.SHN bit (实际不强制) + CC.EN 1→0：
- [`disable()`](../src/controller/mod.rs)：清 sqs/cqs/pending_ios/dual_prp_writes/
  prp_list_ops/compare_ops/sqe_inbox/aen_pending/self_test/sanitize/features/
  doorbell/current_ps/irq_coalesce/host_id_lo
- CSTS.RDY=0
- error_log + self_test_last + sanitize_last_status + fw_slot_revisions
  跨 reset 持久化（spec 行为）
- debug_assert! 让残留多段 op 在 test mode loud fail

driver 看到 CSTS.RDY=0 后认 device down。

---

## 关键 spec 章节速查

| Spec § | Topic | 代码位置 |
|---|---|---|
| § 3.1 | Controller Registers | [regs.rs](../src/regs.rs) |
| § 4.4 | PRP layout | io.rs Read/Write 三档 |
| § 4.6 | CQE format | cmd.rs Cqe |
| § 5.2 | AER | mod.rs fire_aen |
| § 5.11 | Device Self-Test | admin.rs SELF_TEST + tick |
| § 5.14 | Format NVM | admin.rs FORMAT_NVM (Phase K1 真切 LBAF/PI) |
| § 5.16 | Get Log Page | logs.rs build_* |
| § 5.16.1.x | 各 log 子结构 | logs.rs 每 builder doc |
| § 5.17.2 | Identify Ctrl/NS | cmd.rs build_v2_bytes |
| § 5.21 | Features | admin.rs SET/GET_FEATURES + fid module |
| § 5.22 | NS Management | admin.rs NS_MANAGEMENT (Phase K3) |
| § 5.26 | Sanitize | admin.rs SANITIZE (Phase K5) |
| § 5.27/5.28 | Security Send/Receive | admin.rs (Phase L5) |
| § 6.11 | Reservation Acquire | reservation.rs (Phase H6) |
| § 6.13 | Reservation Register | reservation.rs |
| § 6.14 | Reservation Report | reservation.rs build_reservation_report |
| § 6.15 | Reservation Release | reservation.rs |
| § 7 | Directives | admin.rs (Phase L4) |
| § 8.3 | Protection Information | pi.rs T10 DIF CRC16 (Phase K1) |
| NVM CS § 3 | NVM commands | io.rs |
| NVM CS § 4.4 | Compare | io.rs (Phase K2) |
| ZNS CS § 4 | Zone Management | ZNS_DESIGN.md (deferred) |

---

## 排错 / 调试模板

### Driver 卡在 Identify Controller

- 检 `RUST_LOG=pcie_remote_nvme_userspace=debug` 看 admin.rs dispatch_admin
  是否进 IDENTIFY arm
- DMA-write 是否成功（on_dma_complete ok=true）
- buf 长度是否 4096

### Driver 看不到盘 (Get-Disk 无 NVMe)

- 检 OpenHCL kmsg：pcie_remote_device handshake 是否成功
- 检 controller log：是否到 `connected; spawning NvmeController`
- 检 IdentifyController 的 NN > 0

### IO 一直 timeout

- 检 token routing：on_dma_complete 是否找到 pending_ios entry
- 检 CQE 是否真 post (post_cqe log)
- 检 fire_interrupt 是否真发

### SMART data_units 不增

- 检 io.rs WRITE / READ 完成路径是否 `stat_host_writes += 1` /
  `stat_lba_written += nlb`
- 完成路径在 NvmWriteDmaRead / NvmWriteDualPrp ready / NvmWritePrpList done

---

## 扩展练习

读完本教程的读者，可以尝试：

1. **添加 ZNS** — 见 [ZNS_DESIGN.md](../ZNS_DESIGN.md)，独立 example
   `pcie_remote_zns_userspace` 复用 SDK
2. **添加真 PI Write 路径** — 见 [K4_DESIGN.md](../K4_DESIGN.md)，
   interleave data + tuple inline 写 backing file
3. **零拷贝优化** — 见 [M2_MMAP_DESIGN.md](../M2_MMAP_DESIGN.md)，
   memmap2 替换 file.read/write
4. **per-queue 并发 dispatch** — 见 mod.rs 头注释 Phase M3 section
5. **写一个新 PCIe device** — 复制 `pcie_remote_rng_userspace` 模板
