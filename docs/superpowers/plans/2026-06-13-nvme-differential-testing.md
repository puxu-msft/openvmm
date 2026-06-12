# NVMe Controller 语义层差分测试 track（QEMU 活 oracle）

> **状态**：方案已定（POC 证实承重假设、零 blocker，2026-06-13）。D0/D1/D2 **execution-ready**；
> D3 **roadmap**（依赖 D2 落地后的归一化基建，详化 gate 见各段）。
> **POC 证据**：`usnvmemu/experiments/2026-06-13-differential-qemu-poc/RESULT.md`。

## 1. 目标与定位

为 NVMe **controller 命令语义层**补一个**独立 controller 实现**做活 oracle 的差分测试，专抓
现有测试体系抓不到的残余类：

- **现有覆盖**：①`offset_of!`/`nvme_spec` anchor（抓 packed 布局/转写错，LESSON §1）②spec-derived
  独立 oracle + revert-verify（backing 真字节 / `PiTuple::compute` / capture 重组，tests.rs）
  ③跨进程裸 wire e2e（LESSON §7）。
- **残余缺口**：以上三类的"判据"全是**本仓按自己对 spec 的理解写的**。若我们对某条语义的理解
  **整体性错了**，三类都不会报（同源误读）。唯有"另一个独立 controller 实现"能戳破——这正是
  LESSON §2 "self-consistent ≠ spec-conformant" 的真精神推广到 controller 语义。

**定位（采纳 architect 价值排序）**：**窄范围补充，不越过现有路线**。差分主要加固"语义理解整体性
错"这一残余；不替代 anchor/spec-derived oracle。bug 分布显示真正咬人的还有"结构性不可见路径"
（>4GiB 地址 §26、realize 盲区 §28、async 自馈 §29），那些差分也照不到，另行处置。

**铁律**：QEMU ≠ spec。**任何 divergence = 回 spec 裁定谁对的信号，绝不"向 QEMU 看齐"做
bug-compatible**。QEMU 版本钉死 + vendor 记录（§21 复发温床防护）。

## 2. POC 已证（承重假设，不再验）

- 拓扑成立：同一 guest 内核同时驱动 usnvmemu(vfio-user-pci) 与 QEMU 自带(-device nvme)，
  良定义命令两端逐字节一致 → **同一逻辑命令 = 等价 SQE**（中和"不同 wire"异议）。
- passthru 可注入畸形命令；内核拒（ioctl_ret<0）vs controller 拒（≥0）可区分。
- 首跑 14 命令即抓 6 分歧，其中 **DNR-omission 经源码坐实是 usnvmemu 真 gap**
  （`cmd.rs Cqe::error` 错误 CQE 从不置 DNR）。

## 3. 阶段

### D0 — 收割 POC 分歧（execution-ready，预计 ~1 day）

**前置（BLOCK，已解）**：裁定须基于**完整 SQE 字段**（CNS/nsid/CDWn），非昵称。RESULT.md 已回填
「分歧命令完整 SQE 字段」表——D0 直接用它。

把 6 处分歧 + 2 处存疑逐条**对 NVMe Base 2.0 spec 裁定**（**每条分开裁，不笼统归类**），确认是
usnvmemu bug 的修掉：

| 分歧（完整字段见 RESULT.md）| 待裁定 | 预判 |
|---|---|---|
| id_ns_bcast（CNS=0x00, nsid=广播）DNR=0 | §4.6.1 DNR 语义 | SC=0x0b 两端皆对；缺 DNR → 修 |
| admin_badopc / io_badopc（Invalid Opcode 0x01）DNR=0 | §4.6.1 | 永久错误 → 加 DNR |
| io_read_oob（LBA OOR 0x80）DNR=0 | §4.6.1 | 永久错误 → 加 DNR |
| getfeat_resvd FID=0x7f 接受 | FID 0x7f 是否保留区 | 保留→应 Invalid Field，**疑 bug** |
| getlog_unk LID=0xff 接受 | LID 0xC0-0xFF vendor 区语义 | vendor 区，分歧合法可能，留注不强改 |
| id_cns_resvd CNS=0x1f（**共错一致**）| CNS 0x1f 是否定义 | 两端一致但都可能错，**不可入回归基线**，独立查 spec |

**DNR 裁定依据（纪律纠偏，关键）**：spec **没有**"这些 SC 必 DNR=1"的逐码 SHALL 表。DNR 是
**§4.6.1 的语义**——"同命令不改内容重提是否必然再失败"。对**确定性永久错误**（Invalid Opcode/
Field/LBA OOR/Invalid NS）重提必失败 → DNR=1 是语义所要求，usnvmemu 恒 DNR=0 是 conformance bug。
**裁定理由写"§4.6.1 语义 + 该 SC 为确定性永久错误"，绝不写"SHALL 表要求"（不存在的表当依据 =
"拿 QEMU 当 spec"的孪生陷阱）。** D0 落地时对照手头 spec PDF §4.6.1 原文复核一次。

**DNR 修复实现**：
- 单点改 [`Cqe::error`](../../usnvmemu/crates/nvme_firmware/src/cmd.rs)（唯一错误 CQE 构造点，
  224+ 调用点全只传 status u16）按 status 值**查表**置 DNR——**绝不**逐调用点手填（退回 R2d 前
  手填反模式）。
- 查表须**精确区分永久 vs 瞬态**：NAMESPACE_NOT_READY(0x82)/FORMAT_IN_PROGRESS(0x84)/
  SANITIZE_IN_PROGRESS(0x1d) 等瞬态错误**不置 DNR**；且注意同 SC byte 不同 SCT（如 0x1d
  Sanitize-Generic vs Self-Test-Specific）。这张"永久/瞬态判定表"本身配 anchor test 钉死。

**产出**：
- 每条裁定结果（含 spec 章节锚）写入 `RESULT.md` + 确认 bug 的修复走 revert-verify + rust-reviewer
  + 中文 commit。
- **D0 内部允许"快裁先修（DNR/FID）、慢裁挂起"**（如 id_ns_bcast CNS 深查、CNS=0x1f）——慢裁的
  那条不卡住 D1 启动。

### D1 — libnvme 解析侧差分（execution-ready，预计 1 day，便宜优先）

不需第二个 controller。把 usnvmemu 的 Identify/log 输出喂 **libnvme parser**（`nvme id-ctrl -H`
/`id-ns -H`/`get-log`），diff 我们对自己输出的解读 vs libnvme 的解读——直击项目 #1 bug 类
（packed offset/字段错位）。RUNBOOK §0 已有 `nvme id-ns` 解析 LBAF 雏形。

**形态**：在现有 guest-boot harness 内（usnvmemu 单设备即可），guest 装 nvme-cli（或最小
libnvme 解析），跑 `nvme id-ctrl -H /dev/nvmeX` 等，host 侧断言关键字段（VID/SN/MN/版本/
OACS/ONCS/NN/LBAF/MDTS…）与 usnvmemu 源期望一致。

**详化 gate**：nvme-cli 进 initramfs 的成本（动态链接 libnvme/json-c）——若过重，改用 D2 的
静态注入器 raw-dump Identify buffer 到串口、host 侧用 libnvme 库离线解析。POC 已证静态二进制
路线可行，此为 fallback。

**共用 driver（A-2）**：D1 与 D2 共用**同一命令矩阵 driver**（`poc_init.sh` 后继），仅断言侧分流
（D1=解析断言，D2=执行 diff），避免两套语料各自漂移（self-consistent 陷阱的工程版）。

### D2 — 执行侧差分 harness 产品化（execution-ready，预计 2 day）

把 POC 升级为可维护、CI-friendly 的 harness：

- **命令语料策展**：从 POC 14 条扩到一个**有意义完整**的边界/畸形矩阵。每条标注"期望分歧/期望
  一致"，且**期望一致项必须带 spec 锚**（见 §4 D-1）。覆盖：
  - admin：Identify 各 CNS、Get/Set Features 各 FID 含保留、Get Log 各 LID、Abort、保留 opcode。
  - io：Read/Write/Compare/Flush/Write-Zeroes/DSM 的越界/对齐/nlb 边界/保留 opcode。
  - **高价值边界补充（E-1，项目 bug 重灾区）**：
    - **PRP/SGL 边界**——PRP2 非页对齐、跨页 PRP-list、SGL segment chain、PSDT=11 保留
      （前提：确认 `pt_diff.c` 能精确控 prp1/prp2 布局）。usnvmemu 最咬人面。
    - **MDTS 边界**——usnvmemu MDTS=5（128KiB），nlb 刚好=/略超 MDTS，两端差异是真信号。
    - **fused C&W**——usnvmemu 限 ≤1 page（超返 ATOMIC_WRITE_UNIT_EXCEEDED），QEMU 大概率不同。
    - **保留/广播 NSID**（0xFFFFFFFE/0xFFFFFFFF）在 IO vs Identify vs Get Log 上的不同处理。
- **分类输出**：内核拒（ioctl_ret<0）vs controller 拒（≥0）vs 一致 vs 分歧；分歧再分"已知合法
  自由度"vs"待裁定"。**内核拦截类（如 nsid=0，POC 已证 EINVAL 不达 controller）须显式标注
  "不可达 controller，非差分目标"**，绝不当"两端一致"塞进回归（D-3）。
- **最小归一化 mask 内置 D2（E-2，不推迟到 D3）**：SN/MN/FGUID/时间戳/队列数等 ~5 个**铁定合法
  自由**字段做 mask，否则 D2 triage 被 SN/MN 噪声淹没、每次 diff 报一堆假分歧。复杂的可选特性位/
  完成顺序乱序容忍留 D3。
- **读后写 backing 差分（D2 主线只做干净子集）**：统一 LBAF/容量（均 64MiB/512 已对齐），写已知
  pattern 后两端各自回读 + 比对。**PI/metadata 路径的 backing 差分拆出，并入 D3**（依赖统一 LBAF +
  QEMU PI 参数化 `ms/mset/pi/pil`，工程量近 D3 归一化，不塞 D2 主线撑爆 2 day 预算）。
- **QEMU 版本钉死**：harness 记录并断言 QEMU 版本（linuxbrew 11.0.1），版本变即告警 + 触发 §4 D-2
  re-triage。
- **回归基线**：把"期望一致（带 spec 锚）"集合作为回归（任一转分歧 = usnvmemu 改动引入 regression）；
  "待裁定分歧"集合人工 triage 后转入 D0 式裁定。

**产出**：`experiments/` 内的 POC 升格为 `crates/nvme_firmware/scripts/differential_qemu/`（或
保留 experiments + 文档指路），README + ROADMAP 收录；rust-reviewer。

### D3 — 扩展与归一化（roadmap）

**详化 gate：D2 落地 + 至少一轮真 triage 后再详化**（取决于 D2 暴露的归一化痛点真实形状）。
方向（不预定细节）：

- 归一化层（**复杂部分**，最小 mask 已在 D2）：可选特性位 + 完成顺序乱序容忍，做成可声明的
  field-mask 表。
- PI/metadata backing 差分（从 D2 拆入）：统一 LBAF + QEMU PI 参数化 `ms/mset/pi/pil`。
- 语料扩展：reservation 语义、NS management、ZNS（若 QEMU 支持）。

**已砍（A-1）**：~~SPDK 第三 oracle~~——QEMU + libnvme 两个独立 oracle 已覆盖唯一立项理由（语义
理解整体性错）；第三个同类 controller oracle 边际信号≈0，而 PI 路径跛 + hugepage 维护成本是真的。
将来若有具体 bug 指向再单独立项，不留 roadmap 引力。

## 4. 跨阶段纪律（务必照旧）

- 每阶段：implement → 差分/anchor 测试 → **revert-verify**（注 bug 确认红再恢复）→
  `cargo test --lib` + clippy + fmt 绿 → **ecc:rust-reviewer APPROVE（0 C/H）** → 中文 conventional commit。
- spec 值（SC/SCT/DNR/offset/FID/LID 区段）一律对 `nvme_spec` / spec 章节核，**不手算、不信自家
  注释、不拿 QEMU 当 spec、也不拿不存在的 spec 表当依据**（DNR 见 D0 裁定纠偏）。
- **D-1（关键）oracle 校准基线**：回归基线里每一条"期望一致"**必须带 spec 锚（标 spec 章节）**，
  **绝不**凭"两端跑出来一样"入基线——否则"共错一致"项（如 POC 的 CNS=0x1f 两端都接受）会被焊进
  回归、把 bug 锁死。这是 §2「self-consistent ≠ spec-conformant」对**我们自己回归基线**的直接推论。
- **D-2 QEMU 升级 re-triage**：QEMU 版本变更（如 11→12 可能改了某 DNR/SC 行为）时，差异集合**全部
  回到"待裁定"重新人工 triage**，不沿用旧裁定（防 §21「版本复发温床」从传输层复发到语义层）。
- 多会话共享树：只 `git add` 自己文件，pathspec 限定 commit，绝不 `git add -A`。
- 文档同步：改 spec 行为 → 同步 ROADMAP / SPEC_CONFORMANCE / README / RESULT（算"完成"的一部分）。

## 5. 非目标 / 边界

- 不追求"和 QEMU 全字段一致"——只在干净子集（status/result/backing）+ 策展语料上比，其余靠
  归一化或显式 mask。
- 不用差分替代现有 anchor/spec-derived oracle；定位是残余类补充。
- 不处理"结构性不可见路径"（>4GiB/realize 盲区/async 自馈）——差分照不到，另行处置。
