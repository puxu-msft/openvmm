# NVMe-oF RDMA — Wire / Verbs Contract（V9 R0 设计期 oracle 产出）

> **体裁**：字段级 wire 布局 + verbs 语义契约 + 两个设计决策 + binding ADR 草案。这是
> [V9 RDMA detailed plan](../plans/2026-06-14-phase-v9-rdma-detailed.md) 的 **R0 阶段交付物**，
> 喂下游三阶段：**(a)→R1'** verbs 语义；**(b)→R2** `HostBuf` 三态 + capsule 字节布局；
> **(c)→R3** association↔QP↔session 映射。
> **来源权威性**：全部字段/枚举/偏移抽自 Linux 真驱动（NVMe-oF RDMA 一致性参考实现）+ rdma-core，
> 经 2 路独立 subagent oracle 交叉核验。**未写任何实现代码**——本文是契约，非实现。
> **Date:** 2026-06-14 · **平行参照**：TCP 版 [nvme-tcp-wire-reference](2026-06-04-nvme-tcp-wire-reference.md)。
> **字节序铁律**：所有多字节整数 = **little-endian**（`__le16/32/64`）；Rust struct 用 zerocopy 0.8
> `U16<LE>/U32<LE>/U64<LE>`，字段顺序 == 下表字节序，`offset_of!` 锚定，**禁手算 packed offset**
> （LESSONS §17 血泪）。

权威源 URL：
- `include/linux/nvme-rdma.h` / `include/linux/nvme.h`
- `drivers/nvme/host/rdma.c` / `drivers/nvme/target/rdma.c` / `drivers/nvme/target/auth.c`
- rdma-core README（providers / rxe / siw）

---

## A. RDMA-CM Private Data（连接建立协商，喂 R3）

NVMe-oF RDMA 用 RDMA-CM 建连，**每 queue（admin + 每 IO queue）一次独立 `CONNECT_REQUEST`**，
qid 在 private data 里。private data ≤ 56B（CM 上限）。

### A.1 `nvme_rdma_cm_req`（host→target，CONNECT_REQUEST private_data，32B）

| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 0 | `recfmt` | `U16<LE>` | record format，必须 `0x0000`（`NVME_RDMA_CM_FMT_1_0`） |
| 2 | `qid` | `U16<LE>` | queue id；0=admin，≥1=IO |
| 4 | `hrqsize` | `U16<LE>` | host RQ size（**1-based**=条目数） |
| 6 | `hsqsize` | `U16<LE>` | host SQ size（**0-based**=条目数−1，即 sqsize） |
| 8 | `cntlid` | `U16<LE>` | controller id；admin 填 `0xFFFF`/由 Connect 决定，**IO queue 填已分配 cntlid** |
| 10 | `rsvd[22]` | `[u8;22]` | 置 0 |

### A.2 `nvme_rdma_cm_rep`（target→host，accept private_data，32B）

| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 0 | `recfmt` | `U16<LE>` | `0x0000` |
| 2 | `crqsize` | `U16<LE>` | controller RQ size（target 接受深度，**1-based**） |
| 4 | `rsvd[28]` | `[u8;28]` | 置 0 |

### A.3 `nvme_rdma_cm_rej`（target→host，reject，4B）+ status 枚举

| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 0 | `recfmt` | `U16<LE>` | `0x0000` |
| 2 | `sts` | `U16<LE>` | reject status（下表） |

`enum nvme_rdma_cm_status`（**spec 强制全集**）：`INVALID_LEN=0x01` / `INVALID_RECFMT=0x02` /
`INVALID_QID=0x03` / `INVALID_HSQSIZE=0x04` / `INVALID_HRQSIZE=0x05` / `NO_RSC=0x06` /
`INVALID_IRD=0x07` / `INVALID_ORD=0x08` / `INVALID_CNTLID=0x09`。
> **Linux target 实发子集**（实现选择）：仅 `INVALID_RECFMT` / `INVALID_HSQSIZE` / `NO_RSC`。
> emulator **host 解析端须认全集；target 发送端可选严格度**（与我们的 `decide_connect_cntlid`
> conformance 哲学一致——见 detailed plan ⚠️③ 与 static-controller-model）。

常量：`NVME_RDMA_IP_PORT=4420`、`MAX_QUEUE_SIZE=256`、`DEFAULT_QUEUE_SIZE=128`、`NVME_AQ_DEPTH=32`。

### A.4 填充规则（admin vs IO 差异，emulator target 必守）
- **host**：admin `hrqsize=32,hsqsize=31`；IO `hrqsize=queue_size, hsqsize=queue_size−1, cntlid=分配值`。
- **target 校验**：`recfmt≠0`→reject `INVALID_RECFMT`；`recv_queue_size = hsqsize+1`（0→1-based）；
  `send_queue_size = hrqsize`；admin 若 `recv_queue_size>32`→`INVALID_HSQSIZE`；accept 回 `crqsize=recv_queue_size`。
- ⚠️ **off-by-one 陷阱**：`hsqsize` 是 0-based，`hrqsize/crqsize` 是 1-based。
- ⚠️ **两层 cntlid，别分叉（architect review，喂 R3）**：CM private data 的 `cntlid`（§A.1 偏移 8）只是
  **transport 层绑定**（IO QP 绑到 admin connect 创建的 controller）。**conformance 校验仍走 Fabric Connect
  层**——复用 TCP 的 `dispatch_plan::decide_connect_cntlid`（dynamic 0xFFFF / static-any 0xFFFE / 具体匹配，
  见 static-controller-model）。R3 **不要**在 CM 层另造一套 cntlid 校验与 TCP 决策核分叉；CM 层只做 transport
  绑定，授权判定在 Connect。`INVALID_CNTLID=0x09`（§A.3）是 CM 层 transport 错（如 IO QP 指向不存在的
  association），与 Connect 层 conformance reject 是两回事。

---

## B. 命令/响应胶囊 + SGL descriptor 字节布局（喂 R2）

### B.1 command capsule = 64B SQE（经 RDMA SEND）
data_ptr union 位于 **SQE byte [24..40]**（16B）。RDMA 路径放 keyed-SGL 或 inline-SGL（**非 PRP**）。

### B.2 Keyed SGL Data Block descriptor（16B，RDMA READ/WRITE 寻址）

| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 0 | `addr` | `U64<LE>` | 远端虚拟地址 / MR iova |
| 8 | `length[3]` | `[u8;3]` | **LE 24-bit**（`put_unaligned_le24`，单描述符 ≤16 MiB） |
| 11 | `key[4]` | `[u8;4]` | **LE 32-bit** rkey |
| 15 | `type` | `u8` | 高 4 bit=type，低 4 bit=subtype |

type/subtype nibble（**原始值未预移位**）：type `KEY_SGL_FMT_DATA_DESC=0x4`；subtype `ADDRESS=0x0` /
`INVALIDATE=0xF`。host 实写：**FR-MR 带失效请求=`0x4F`**（`(0x4<<4)|0xF`）；**全局 rkey 不失效=`0x40`**（`(0x4<<4)|0x0`）。
> emulator 作 **target（收端）** 须 parse host 送的 type/subtype 字节并 reject 未知值——收端 subtype 全集待 R1' 对 nvmet-rdma 行为核实（§I）。

### B.3 Inline SGL descriptor（`NVME_SGL_FMT_OFFSET`，16B，**布局不同于 keyed**）

| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 0 | `addr` | `U64<LE>` | inline 模式 = **icdoff**（不是真地址！） |
| 8 | `length` | `U32<LE>` | **32-bit**（区别于 keyed 的 24-bit！） |
| 12 | `rsvd[3]` | `[u8;3]` | 保留 |
| 15 | `type` | `u8` | inline = `(0x0<<4)|0x1` = **`0x01`** |

数据本身紧跟 64B SQE 之后在同一 SEND capsule 内。

### B.4 response capsule（含 16B CQE）+ SEND_WITH_INV
- target 决策：命令 SGL subtype 含 `INVALIDATE(0xF)` → `rsp.invalidate_rkey = sgl.key` →
  响应用 **`IB_WR_SEND_WITH_INV`**（帮 host 远程失效 MR，省 host 一次 LOCAL_INV）；否则普通 `IB_WR_SEND`。
- host 收响应：recv WC 带 `IB_WC_WITH_INVALIDATE`=target 已失效、host 无需动作；否则 host 自 post `LOCAL_INV`。

---

## C. inline data 容量协商（喂 R2/R3）

### C.1 Identify Controller 字段（emulator 必须广告正确）
| 偏移 | 字段 | 类型 | 含义 |
|---|---|---|---|
| 536 | `sgls` | `U32<LE>` | SGL 能力位（见下） |
| 1792 | `ioccsz` | `U32<LE>` | IO Command Capsule Size（单位 **16B**） |
| 1796 | `iorcsz` | `U32<LE>` | IO Response Capsule Size（单位 16B） |
| 1800 | `icdoff` | `U16<LE>` | In-Capsule Data Offset（单位 **16B**） |
| 1803 | `msdbd` | `u8` | Max SGL Data Block Descriptors |

`sgls` 位：`KSDBDS=1<<2`（Keyed SGL Data Block，**RDMA target 必置**）、`MSDS=1<<19`（metadata SGL，
与邻居 scarlet-lynx SGL×PI 一致）、`SAOS=1<<20`。

### C.2 容量算法 + host 选 inline 的阈值
- in-capsule data size = `ioccsz×16 − 64`。target `inline_data_size` 默认 `PAGE_SIZE`（4096），max `16K`。
- host **全部满足才 inline**：①`count≤4` 段 且 ②`WRITE` 方向 且 ③**仅 IO queue（admin 不 inline）** 且
  ④`use_inline_data` 启用 且 ⑤`payload ≤ inline_data_size`。否则走 FR-MR keyed SGL（`0x4F`）。

---

## D. verbs 语义契约（喂 R1' `trait RdmaVerbs`，防 mock 自洽地错）

> **这是 mock-first 的命门**：`MockRdma` 必须模拟以下真语义，**而非乐观 FIFO-永不失败**
> （否则 R5 真 verbs 一接全改，[[review-not-optional-self-consistent-trap]] 重演）。

- **D.1 RC QP completion 保序**：同 QP 上 post 的 WR **按 post 顺序处理/完成**。target 把 SEND 链为
  RDMA WRITE chain 的 `next`、一次 `ib_post_send` 提交整链——**靠链表顺序、非 fence**：WRITE 先完成、
  SEND 后完成，host 收 CQE 时数据已落地。带 PI 则等 write_cqe 完成做校验、再单独 post SEND。
  **mock 必须实现「同 QP 保序」契约。**
- **D.2 错误语义**：`IBV_WC_*` 错误（`RNR_RETRY_EXC_ERR`/`REM_ACCESS_ERR`/`WR_FLUSH_ERR`）→ **RC QP 进
  `IB_QPS_ERR`，所有 in-flight WR 以 `WR_FLUSH_ERR` 完成 → 必须重建 QP（不能原地恢复）**。drain 用
  `ib_drain_qp`（故 CQ 深度都 `+1 for drain`）。**mock 必须建模 QP-error-state + flush + 重建。**
- **D.3 RNR / recv 池**：target 预投 `recv_queue_size` 个 recv WR；**响应前先 repost**
  （`queue_response` 内，保证下条命令 buffer 就绪）；耗尽→host SEND 撞 **RNR NAK**，按 `rnr_retry=7` 重试。
- **D.4 IRD/ORD**：RDMA READ 由 **target 发起**（读 host 内存做写命令数据入）。target accept 时
  `initiator_depth=min(req, max_qp_init_rd_atom)`、`responder_resources=0`；host `responder_resources=
  max_qp_rd_atom, initiator_depth=0`。约束 target 并发 RDMA READ 深度。
- **D.5 QP/CQ 模型**：每 queue=1 QP+1 CQ（send/recv 共享），`IB_QPT_RC`。target CQ 深度公式
  **`recv_queue_size + 2×send_queue_size`（+1 drain）**；`max_send_wr=send_queue_size+1`。SRQ 可选
  （`use_srq`，`srq_size≤1024`）；**host 不用 SRQ**。`NVMET_RDMA_MAX_MDTS=8`。

---

## E. 设计决策①（喂 R2 前置 gate）— `HostBuf` 类型

`FabricBackend` 抽象「把 controller 的一段数据在 host 内存 ↔ 本地 buffer 间移动」时，
**host 缓冲的表达必须覆盖全部 spec-强制形态**。R0 在此定死字段，使 R2 不盲定日后要改的类型（architect review gate ⑤）。

> **architect review 纠正**：初版三态漏了 **multi-SGL descriptor list（`msdbd>1`）** 与 **separate-meta PI 段**
> 两个 spec-强制形态——而 §C 自己广告了 `msdbd`/`MSDS`，单元组 `Keyed{addr,rkey,len}` 与之内部矛盾
> （host 送 list 时第二段是**全新 addr+rkey**，非 `addr+offset`）。这是 [[fuzz-the-contract-not-current-impl]]
> 的设计期版：契约（广告值）比建模宽，宽出来的藏 latent 缺口。**故改 segments 模型**（N=1 即常见 fast-reg
> 单描述符路径，零额外复杂；N>1 与 separate-meta 自然容纳，类型不再需要改）。

```rust
/// host 数据缓冲的位置——全 transport 形态（spec-complete）。R2 定 FabricBackend 前必须锁死。
pub enum HostBuf {
    /// TCP：无远端句柄，靠 R2T/H2CData PDU 流控；只有逻辑长度。
    FlowControlled { total_len: u32 },
    /// RDMA keyed-SGL：target 发起单边 RDMA READ(写命令)/WRITE(读命令)。
    /// `data` = 数据段（msdbd=1 时 len()==1=常见 fast-reg 路径；msdbd>1 时 SGL list 多段）。
    /// `meta` = separate-meta PI 的独立 PI 段（`MSDS` 能力；inline-meta / 无 PI 时 None）。
    Keyed { data: Vec<KeyedSeg>, meta: Option<KeyedSeg> },
    /// RDMA in-capsule inline：数据已在收到的 capsule buffer 内（icdoff 偏移）。
    Inline { capsule_offset: u32, len: u32 },
}
/// 一个 keyed 远端区域。**不变式：`len ≤ 0xFF_FFFF`（24-bit，§B.2）**。
pub struct KeyedSeg { pub remote_addr: u64, pub rkey: u32, pub len: u32 }
```

**承重不变式 + enforce 责任方（architect review 补）**：
- `KeyedSeg.len` 寄居 `u32` 但 wire 仅 24-bit → **解析入站 capsule SGL 时（parse-don't-validate 边界）校验
  `len ≤ 0xFF_FFFF`，超界 reject SC=0x18**（`Inline.len` 才是真 32-bit，§B.3——两者类型同、值域不同，
  正是 §H#1 陷阱的类型层投影）。R2 `FabricBackend` 把关。
- **零数据命令**（Flush / Write-Zeroes-without-data 等）：**不挂 `HostBuf`**（命令无数据传输），非挂 len=0 variant。

**与纠正①（token-FIFO + cumulative offset）的接合**：每命令从入站 capsule 抽 `HostBuf` 挂命令上下文；
controller 逐 fragment 发 `dma_read/dma_write(sentinel,len)` 时，桥按 token-FIFO **cumulative offset** O
定位：在 `data` 段内 O 推进、跨段切换到下一 `KeyedSeg`（**O 不再是单段 `addr+O`，而是 list 内全局偏移→
定位到某段 + 段内偏移**）→ `RDMA op(seg.remote_addr + 段内偏移, seg.rkey, L)`；`meta` 段独立处理（对应
controller 的 MPTR/PI 子-DMA，与 scarlet-lynx SGL×PI separate-meta 模型一致）；`Inline` 直接从 capsule
buffer 取。**TCP impl 只匹配 `FlowControlled`**（其余 `unreachable!`）；RDMA impl 匹配 `Keyed`+`Inline`。

> **CQE 投递路径差异（architect review，喂 R4）**：TCP 桥靠 `CQ_BASE_GPA` 哨值识别 controller 的
> `post_cqe`→`dma_write` 当 CQE 字节转 CapsuleResp。**RDMA 下 CQE 走 response capsule（§B.4）经 SEND/
> SEND_WITH_INV**，不是 dma_write 到 sentinel GPA。故 RDMA 桥的 `post_cqe` 拦截转「构造 16B CQE response
> capsule + SEND」，data DMA（sentinel）才转 RDMA READ/WRITE——两条拦截路径在 RDMA 下分流（data=RDMA verb /
> CQE=SEND capsule），R4 落地须区分。

> **M1/M2 分层**：M1 可先实现 `data.len()==1` 单描述符（fast-reg 常见路径）+ inline + inline-meta；
> multi-segment list（`msdbd>1`）与 separate-meta 段是 M1 的自然扩展（类型已容纳、无需改）。**但 `msdbd`/`MSDS`
> 广告值须与当下真实现能力一致**（M1 单段则广告 `msdbd=1`，别广告 8 却只收 1 = conformance 缺口）。
> R1' POC 须验：`poll_cq` 完成顺序 ↔ keyed-SGL fragment 发射顺序对应（detailed plan R-1）。

---

## F. 设计决策②（喂 R3）— association ↔ QP ↔ session 映射

**决策：per-QP session 实体（镜像 TCP 的 per-conn-per-qid），association = 共享 `NvmeController`。**

依据：
- RDMA 每 queue 一次独立 `CONNECT_REQUEST`（qid 在 CM private data，§A）——与 TCP「每 qid 一条 conn」
  **同构**。现有 session 模型已是 per-conn 单 qid（`current_qid`）。
- 现有 bin 已用 `SharedControllerInner = Arc<parking_lot::Mutex<NvmeController>>` 让**多 conn（多 qid）
  共享一个 controller**（ADR-003）。RDMA：一 association 的多个 QP（qids）共享同一 `Arc<Mutex>`，
  由 CM private data 的 `cntlid`（IO-queue connect 填）绑定到 admin connect 创建的 controller。

**映射结论**：
| 层 | 实体 | 复用 |
|---|---|---|
| association | 共享 `NvmeController`（keyed by cntlid） | **`SharedControllerInner` 原样复用，零改** |
| QP（admin / 每 IO queue） | 一个 `FabricBackend` + 一条 per-QP pump 循环 | 镜像 TCP per-conn → **`FabricBackend` 是 per-QP，非 per-association** |

**化解 architect 的「日后 association-level 资源返工」担忧**：
- **shared CQ / SRQ**：是 verbs 层资源优化（多 QP 共享一个 CQ/SRQ），**位于 `RdmaVerbs` 抽象之下**，
  与 per-QP session 正交。`RdmaVerbs` impl 内部可 poll 共享 CQ 再把 completion 派发给对应 QP 的 session。
  **决策：M1 用 per-QP CQ（简单）；shared-CQ/SRQ 作 M2 优化，不动 session 层。**
- **auth 跨 admin+IO queue**：CHAP 在 admin QP connect 上做（association 级），IO QP connect 校验 cntlid
  授权——association 级 auth 状态本就活在共享 controller 里（与 TCP 一致）。per-QP 不阻碍。

⇒ **per-QP 决策不强迫 session 层返工**；association 级资源沿用已证基础设施。

**R1' open questions（architect review，§F 论证依赖但 R0 不定细节，列给 R1' 定 `trait RdmaVerbs`）**：
1. **completion→QP-session 派发原语**：`poll_cq` 返回粒度 = per-QP CQ（M1 倾向，简单）还是 shared-CQ-then-demux？
   `wr_id ↔ (QP, command, fragment)` 映射谁维护？倾向：M1 per-QP CQ，`RdmaVerbs::poll_cq` 返回本 QP 的
   completion（含 `wr_id` 关联回 token），session 不暴露 QP handle 给上层；shared-CQ 留 M2（verbs 层内部 demux）。
2. **per-QP ↔ cntlid 绑定时机**：IO QP 的 CM private data 带 cntlid（§A.1）→ 绑到哪个 `SharedControllerInner`
   的时机（CONNECT_REQUEST 解析时 vs Fabric Connect 时）。倾向：CM 层按 cntlid 选 controller、Connect 层做
   conformance 校验（见 §A.4 两层 cntlid）。

---

## G. binding 选型 ADR 草案（→ 落地时升 crate `DECISIONS.md` ADR-008）

**筛子**：NVMe-oF RDMA 强依赖 **rdma_cm**（listen/accept/get_cm_event + CM private_data 收发）——
只有 verbs 无 rdma_cm 的 binding **不够用**。

| crate | rdma_cm | 维护 | license | 裁定 |
|---|---|---|---|---|
| **`sideway`**（RDMA-Rust） | ✅ 完整（源码验证 server-side accept + `ConnectionParameter.private_data` + `SEND_WITH_INV`） | ✅ 最活跃（2026-06 仍提交） | MPL-2.0（file-level copyleft，链接安全） | **主选** |
| **`rdma-sys`**（datenlord） | ✅ 全（bindings 含 `rdma_cma.h`） | ⚠️ 停滞但 sys 层贴 ABI 稳定 | MIT | **兜底**（裸 FFI + 自写 CM event-loop） |
| `ibverbs`（jonhoo） | ❌ 无 CM | ✅ | MIT/Apache | 出局（无 CM） |
| `async-rdma` | ✅ | 停滞 | **GPL-3.0** ☠️ | 出局（license 毒药，stars 最高也不可用） |
| `rrddmma` | ❌（自建 TCP OOB 非 rdma_cm wire） | 半活 | MIT | 出局（wire 不兼容 NVMe-oF） |

**决策**：`sideway`（主，经 `trait RdmaVerbs` 薄封）+ `rdma-sys`（锚定兜底，裸 FFI 退路）。两者底层同为
rdma-core C 库，ABI 一致，换实现只动 backing crate 不动 trait。
**理由**（基于正确性/CM 覆盖/可控，非省事）：sideway 是唯一兼具完整 rdma_cm + 活跃 + 受控 unsafe 的安全封装。
**可换性**：trait 隔离。**未锁死单一实现前不删兜底**（5 项 smoke 验过再定，见 §I）。

---

## H. 字节级陷阱清单（emulator 写 struct 时最易错）

1. **keyed length=24-bit（`[u8;3]`）vs inline length=32-bit（`U32`）**——偏移 8 之后两 struct 完全不同布局，混用全错位。
2. **type 字节高低 nibble**：枚举值是**未移位原始 nibble**。keyed FR=`(0x4<<4)|0xF=0x4F`、inline=`(0x0<<4)|0x1=0x01`；直接 OR 未移位 type 得 `0x4`/`0x0`=错。
3. **`hsqsize` 0-based vs `hrqsize/crqsize` 1-based**：target 必 `recv_queue_size=hsqsize+1`。
4. **全 LE**：addr/length/key/recfmt/qid/cntlid 全 little-endian，zerocopy `U*<LE>`，`offset_of!` 锚定。
5. **`cntlid` 仅 IO queue 填**。
6. **`icdoff` 单位是 16B**（非字节）；inline SGL 的 `addr` 放的是 icdoff 值非真地址。
7. **invalidate rkey 流向**：host 在 keyed subtype 置 `INVALIDATE(0xF)` 请求失效；target 把该 SGL 的
   `key` 抄进响应 `SEND_WITH_INV.invalidate_rkey`。
8. **reject status 子集**：host 解析端认全集 0x01..0x09，target 发送端可选严格度。

---

## I. R1' smoke 验证清单（元数据无法确认、须真试编/真 verbs）

1. `sideway` server-side 被动 `accept`/`get_cm_event` 在 soft-RoCE(rxe) 上能否完整跑通（example 偏 client）。
2. `sideway` 在 pinned **1.95** 上能否 build（依赖 `rdma-mummy-sys`+bindgen，需系统装 `librdmacm-dev`/`libibverbs-dev`）。
3. `sideway` 的 `ConnectionParameter::private_data` 能否**精确控制字节**（NVMe-oF `nvme_rdma_cm_req/rep` 要求精确布局）。
4. `IBV_WR_SEND_WITH_INV` + invalidate-rkey 在 sideway 封装下是否 public 可表达。
5. `rdma-sys` 在 1.95 + 当前 rdma-core 的 bindgen 是否干净。
6. **`poll_cq` 完成顺序 ↔ keyed-SGL fragment 顺序**（纠正① / R-1，需真 verbs 或 nvmet-rdma 行为确认）。
7. **CQ 深度公式 `recv+2×send (+1 drain)` 真验**（architect review）：§D.5 是 Linux 实现选择非 spec 强制，
   真 verbs 上须验不 overrun/underrun（依赖我们 SEND/WRITE chain 实际 WR 数）。
8. **收端 keyed-SGL subtype 全集**（architect review）：emulator 作 target 要 parse host 送的 subtype
   （§B.2）；host 发送端值（`0x4F`/`0x40`）是单向视角，收端要认哪些、未知如何 reject，须对 nvmet-rdma 核实。
9. **inline 接收路径真验**（architect review）：§I#6 只验 keyed fragment 顺序；须另验 target 同时正确处理
   **inline-capsule-data**（§B.3，数据在 capsule 内）与 keyed-SGL 两路径。

---

## J. 下游引用（本契约喂谁）
- **R1'**（`RdmaVerbs`+`MockRdma`）：§D 语义契约 + §G binding + §A CM 字节。
- **R2**（`FabricBackend` 泛化）：§E `HostBuf` 三态（前置 gate）+ §B capsule/SGL 布局。
- **R3**（CM Connect + 多 QP + admin）：§A CM private data + §F association↔QP↔session + §C 协商。
- **R4**（数据路径）：§B keyed/inline + §H 陷阱 + §E 接合纠正①。
