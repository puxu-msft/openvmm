# 差分测试 POC（QEMU NVMe 活 oracle）— RESULT

**日期**：2026-06-13
**结论**：**承重假设全部成立，零 blocker；首跑即抓到 6 处真语义分歧（含源码坐实的 DNR-omission conformance gap）。差分 track 立项依据充分。**

## 背景

一位 reviewer 建议：对 QEMU NVMe / SPDK 做"活 oracle"差分——同一命令流喂 usnvmemu 与
QEMU NVMe，diff CQE/backing；抓"两边各自 self-consistent 但语义分歧"的 bug（anchor 常量
抓不到，LESSON §2 的真精神）。本 POC 验其承重假设并量信号。

经 architect 对抗性核验 + 独立查 QEMU 文档，评估被修正/增强：
- vfio-user **传输层**早有三层差分 oracle（libvfio-user C client + 真 QEMU 11），已抓 4 bug；
  reviewer 的新靶子是 **NVMe controller 语义层**（此前只有 known-answer / offset anchor /
  spec-derived 独立 oracle，**无另一个独立 controller 实现**做对端）。
- QEMU NVMe **支持 PI/metadata**（`ms/mset/pi/pil/pif`，仅 32-bit guard 不支持）——原"PI 区
  QEMU 最差 oracle"判断（§21 同型误判）被推翻：QEMU 在 PI 区反是少数可用开源 oracle。
- 关键拓扑修正（architect #5）：同一逻辑命令在 fabric-NS vs 本地-PCIe-NS 上 initiator 翻译成
  **不同 wire**，不是同一命令流。**解法**：让同一 guest 内核同时驱动两者都呈现为本地 PCIe
  （usnvmemu 走 vfio-user-pci，QEMU 走自带 -device nvme）→ SQE 按构造等价。

## 拓扑

复用 `crates/vfio_user_transport/scripts/qemu_interop/` 的 guest-boot 基建（缓存 Ubuntu 6.8
bzImage + busybox + nvme 模块）。一个 QEMU q35 guest（KVM）内：

```
guest kernel nvme 驱动
   ├── /dev/nvme0  = QEMU 自带 -device nvme（独立 C 实现，差分对端）
   └── /dev/nvme1  = usnvmemu（vfio-user-pci，被测）
        ↑ 同一内核、同一本地 PCIe 提交路径 → 同一逻辑命令 = 等价 SQE
```

注入器 `pt_diff.c`（静态编译）：对单设备发一条**字节级可控**的 NVMe passthru 命令
（`NVME_IOCTL_ADMIN_CMD/IO_CMD`），打印 `status(SCT|SC,去 phase) / result(CDW0) / ioctl_ret /
errno`。`poc_init.sh` 对两端发同一 14 条矩阵，host 侧 `run_poc.py` 解析 diff。

## 结果（14 命令 / 6 分歧）

| command | usnvmemu | qemu | 解读 |
|---|---|---|---|
| id_ctrl / id_ns / getlog_err / getfeat_arb | 0x0000 | 0x0000 | 一致成功（**证 H1：SQE 等价**） |
| id_cns_resvd (CNS=0x1f) | 0x0000 | 0x0000 | 两端都接受未知 CNS（一致，存疑留查） |
| io_read_ok / io_flush | 0x0000 | 0x0000 | 一致成功 |
| **id_ns_bcast** (Identify nsid=广播) | 0x000b | 0x**4**00b | 同 SC=0x0b，QEMU **+DNR** |
| **getlog_unk** (LID=0xff) | 0x0000 | 0x4002 | usnvmemu **接受**未知 log；QEMU Invalid Field+DNR |
| **getfeat_resvd** (FID=0x7f) | 0x0000 | 0x4002 | usnvmemu **接受**保留 FID；QEMU Invalid Field+DNR |
| **admin_badopc** (opcode 0xfe) | 0x0001 | 0x4001 | 同 SC=0x01 Invalid Opcode，QEMU **+DNR** |
| **io_read_oob** (slba 越界) | 0x0080 | 0x4080 | 同 SC=0x80 LBA OOR，QEMU **+DNR** |
| **io_badopc** (io opcode 0xff) | 0x0001 | 0x4001 | 同 SC=0x01 Invalid Opcode，QEMU **+DNR** |
| io_read_ns0 (nsid=0) | ioctl_ret=-1 EINVAL | -1 EINVAL | **内核**对称拒绝（passthru 不透传，未误报） |

`0x4000`（status 已 >>1 去 phase 后）= **DNR(Do Not Retry) 位**（raw bit15）。

## 分歧命令完整 SQE 字段（裁定输入，从 `poc_init.sh` 矩阵反解）

裁定 DNR/接受-拒绝 须基于完整命令字段，非昵称。下表是 6 处分歧 + 2 处存疑的精确 SQE：

| command | opc | nsid | CDW10 | 解码 | dir/len |
|---|---|---|---|---|---|
| id_ns_bcast | 0x06 | **0xFFFFFFFF** | 0x00000000 | **CNS=0x00**(Identify Namespace), CNTID=0 | r/4096 |
| getlog_unk | 0x02 | 0 | 0x000F00FF | **LID=0xFF**(vendor 区 0xC0-0xFF), NUMDL=15 | r/64 |
| getfeat_resvd | 0x0A | 0 | 0x0000007F | **FID=0x7F**(保留区), SEL=0 | n |
| admin_badopc | 0xFE | 0 | 0 | 未支持 admin opcode | n |
| io_read_oob | 0x02 | 1 | 0xFFFFFFFF (+CDW11=0x0000FFFF) | slba=0x0000FFFF_FFFFFFFF, nlb=0 | r/512 |
| io_badopc | 0xFF | 1 | 0 | 未支持 io opcode | n |
| id_cns_resvd（存疑·共错一致）| 0x06 | 1 | 0x0000001F | **CNS=0x1F**(保留/未定义) | r/4096 |
| io_read_ns0（内核拦截）| 0x02 | **0** | 0 | nsid=0，内核 EINVAL 未达 controller | r/512 |

**裁定要点**：
- id_ns_bcast 的 **CNS=0x00**（Identify Namespace）下广播 NSID 非法 → SC=0x0b 两端皆对；分歧仅
  在 DNR。（若是 CNS=0x01 Identify Controller 则 spec 忽略 NSID，那才需先质疑 SC 本身——此处不是。）
- id_cns_resvd CNS=0x1F 两端**都接受**=「共错一致」嫌疑，不可凭"两端一样"入回归基线，须独立对 spec 裁定。

## 两类真信号

1. **DNR-omission（4 例：id_ns_bcast / admin_badopc / io_read_oob / io_badopc）**：同 SC，QEMU 对
   永久性错误置 DNR=1、usnvmemu 恒 DNR=0。**源码坐实**：[cmd.rs `Cqe::error`](../../crates/nvme_firmware/src/cmd.rs)
   只从 `SC|SCT` 派生 SF，错误常量从不含 0x8000 → 所有错误 CQE 都不设 DNR。spec 对
   Invalid Opcode / LBA OOR 等"重提不会成功"的错误，DNR 应为 1。**anchor 测试只验 SC 值、
   self-consistent 测试只断言自家产出的 SC，二者都抓不到这一位**——正是 reviewer 预言的类。

2. **过度宽松接受（2 例：getlog_unk LID=0xff / getfeat_resvd FID=0x7f）**：usnvmemu 对未知 log /
   保留 FID 返成功，QEMU 返 Invalid Field。FID=0x7f 属保留区→应 Invalid Field（usnvmemu 疑似 bug）；
   LID=0xff 属 vendor 区→分歧较模糊，需对 spec 裁定（**差分铁律：divergence=查 spec 信号，非
   "向 QEMU 看齐"**）。

## 承重假设裁定

- **H1（同命令→等价 SQE）成立**：所有良定义命令（id_ctrl/id_ns/log/read/flush）两端逐字节一致。
- **H2（畸形可等价注入 + passthru 透传）成立**：14 条中 13 条到达 controller；唯 nsid=0 被**内核**
  在提交前对称拒（EINVAL），harness 据 ioctl_ret<0 正确未误报。教训：注入器须区分"内核拒"
  （ioctl_ret<0）vs"controller 拒"（ioctl_ret≥0）——已实现。

## 已知局限（全套方案须处理）

- 归一化：本 POC 只比 status/result（最干净子集）；Identify/log 全字段差分须 mask 合法自由度
  （SN/MN/可选特性位/队列数）+ 完成顺序，工程量大头在此。
- oracle 不可信：QEMU≠spec，须钉死并 vendor QEMU 版本（§21 复发温床）；分歧一律回 spec 裁定。
- backing 差分：本 POC 未做读后写字节比对（需统一 LBAF/容量，均 64MiB/512 已对齐，下一步加）。

## 运行

```bash
cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/fetch_guest_kernel.sh   # 一次，缓存
python3 usnvmemu/experiments/2026-06-13-differential-qemu-poc/run_poc.py
```

## 产物

- `pt_diff.c` — 静态 NVMe passthru 注入器
- `poc_init.sh` — guest 内命令矩阵
- `run_poc.py` — 双-NVMe harness + diff 表
