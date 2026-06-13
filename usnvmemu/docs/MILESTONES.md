# usnvmemu 里程碑历史档 (Milestones Archive)

> **本文件来历（2026-06-12 整理）**：原先散落在 auto-memory 里的「阶段完成 / 里程碑」记录已**成为历史**——它们记的是做过的工作。按用户原则「记忆文件成为历史后，应挪进项目文档目录，尽可能不丢失正确信息、保留教训与带注解的错误判断」，本文件把那些历史里程碑的叙事、commit hash、当时的教训与走过的弯路（带注解）集中迁移到此。
>
> **分工**：
> - **本文件 (MILESTONES.md)** = 「做过什么 + 当时怎么学到的」叙事档（含 commit hash 与带注解的错误判断）。
> - [LESSONS.md](LESSONS.md) = 可复用技术教训（§17–§30+），本文件深层教训处指向它。
> - [DECISIONS.md](DECISIONS.md) = ADR 决策。
> - [ROADMAP.md](ROADMAP.md) = 动态待办 + §6 历史 phase plan 索引（NVMe-oF V 系列 plan 文件逐条索引在那里）。
> - 各 phase 详细 plan/spec 在 `docs/superpowers/plans|specs/` 与 `crates/<X>/docs/`。
>
> 行事风格、用户偏好、流程教训这类**可迁移的 standing 规则**仍留在 auto-memory，不在本文件。

---

## 1. Firmware 核心里程碑

### 1.1 NVMe 2.0 userspace controller — production-ready-as-teaching（Phase A→Q12 + R + S）

`docs/superpowers/examples/pcie_remote_nvme_userspace/`（后迁为 `nvme_firmware`）完成 NVMe 2.0 controller 教学示例的全面实现 + **12 轮 rust-reviewer 修复**。用户长期目标是教学性 NVMe SSD emulator，要 functional coverage + 性能优化 + 清晰模块化（用户原话「应该都做」）。

- **67 tests passing**（Phase R+S 后）；`cargo clippy --all-targets -- -D warnings` 全绿；`cargo fmt --check` 全绿。
- WSL → Windows MSVC 交叉编译一行命令编出 5.4 MiB PE32+ exe（`bash docs/superpowers/scripts/build-windows-cross.sh pcie_remote_nvme_userspace`）。
- **0 CRITICAL / 0 HIGH / 0 MEDIUM**（12 reviewer rounds + S2-S7 polish）。
- Phase 覆盖 A → Q12 + R1/R3/R4 + S1→S7：
  - K4a/b/c/c-list（PI Write/Read 单+多 LBA + PRP-list）
  - L1/L1c/L1d/L1f（ZNS 基础 + Identify + PI+ZNS plain WRITE）
  - M1b/M2/M-2/M3（IRQ coalesce + mmap zero-copy + ZNS SWR + 多线程 ADR）
  - O1/O2/O3（Simple Copy + Fused C&W detect + 真 atomic chain）
  - P1（Endurance Group + NVM Set + PTPL reservation 持久化）
  - Q1-Q12（PRACT semantics / ZONE_APPEND on PI / Telemetry / ANA / Boot Partition / Lockdown / Crypto Erase / Reservation Notification Mask / DeviceCtx mock / enable+mmio split / WSL cross-build）
  - R1+R3+R4（SGL inline Data Block PSDT=01 + Identify Controller sgls 公布 + RBAR_DESIGN ADR）
  - S1-S7（NS Write Protection Feature 0x84 + SC 0x20 / COPY CONFLICTING_ATTRIBUTES SC 0x80 + Identify AWUN/AWUPF/ACWU + Identify NS NAWUN/NPWG family / NS Attachment 0x15 真 Attach/Detach state + SC 0x18/0x19 / CNS 0x12+0x13 Controller List / Reservation Notification Log / ANA state machine + change AEN type=2 info=3 log=0x0C）
  - H-3（mod.rs 1614 行 on_dma_complete 拆 controller/completion.rs）
- 当时已知未做的边界（文档化在 README）：CMB/PMR memory region（需 BAR2/4 separate window，SDK 不支持）/ Predictable Latency Mode / 真 Boot Partition image 服务 / Multi-thread per-queue dispatch（ADR 解释为何 defer）/ SGL R2 真路径 / PTPL-style NSWP 持久化（WPS=1/3 重启会丢失）。

> 与「教学=严谨全面而非玩具」的标准互为正面样板（见 auto-memory `teaching-means-rigor-not-toy`）。

### 1.2 SGL R2（完整 scatter-gather，PSDT=10）

2026-06-10：`nvme_firmware` SGL R2 全段完成，5 commits `83b0cdf4`→`0e2dfccb`（branch `feat/pcie-remote-experimental`）。

- **R2a**(`83b0cdf4`) 单 segment 多 Data Block scatter/gather：`resolve_data_pointers` 改返 `DataPointer{Prp,SglSegment}` 枚举分流；新 `SglOp`/`SglPlanFrag` 累积器 + `PendingOp::NvmSgl{Fetch,Data}`，镜像 `prp_list_ops` 机件；READ 先读 backing 再按 fragment `dma_write`(scatter)，WRITE `dma_read`(gather) 填后写 backing。仅 plain NS。
- **R2b**(`059de7cd`) Segment chain：`NvmSglFetch{op_id,is_last}` 递归 fetch（末位 continuation），`walk_offset` 跨段累积；`MAX_SGL_SEGMENTS=64` 防自环；`validate_segment_pointer` 共用谓词。
- **R2c**(`9240ff87`) Bit Bucket（spec §4.4.1 方向相关）：READ advance offset+discard（不 dma），WRITE 视 length 为 0。
- **R2d**(`2babf32a`) SGLS bit16 回填(0x0001_0001) + **SGL SC 校正锚定 nvme_spec**。
- **关键产物**：`nvme_firmware/tests/openhcl_pcie_remote_e2e.rs` 跨进程真-firmware 测试（差分 oracle + 直读 backing 独立 oracle + revert-verify 每段实测）；`sgl_status_codes_match_nvme_spec` anchored 测试。100 lib + 16 e2e pass，clippy 0。每段过 ecc:rust-reviewer。

**带注解的错误（R2d 附带挖出并修的 R1 bug）**：SGL SC 全手填错(0x14-0x17) + `SANITIZE_IN_PROGRESS=0x12` live wire bug（应 0x1d Generic）；靠 anchor 到 `nvme_spec::Status` 挖出。**sc 模块结构性重构**(commit `2a6d71ec`)：审计发现 SCT 在 224 个 `Cqe::error` 调用点手填、多处填错（INVALID_PROTECTION_INFO/NS_WRITE_PROTECTED/PI media/ZNS/RESERVATION_CONFLICT/LOCKDOWN/AER）。结构性根治：`sc::` 常量改全 u16 status（SC+SCT<<8 镜像 nvme_spec::Status），`Cqe::error(.., status: u16)` 单参 SCT 自动派生，footgun 消除。`sc_constants_match_nvme_spec` 全量 anchor。102 lib + 16 e2e。全 sc 模块审计完成，无遗留。详 LESSONS §25/§26。

### 1.3 A1–D + B6b（Abort / Reservation Preempt / MSET / MDTS / persistent features / separate-buffer PI）

2026-06-11 一个超长会话在 `nvme_firmware` 交付（全 ecc:rust-reviewer APPROVE 0 C/H，分支 `feat/usnvmemu`）：

- **A1** Abort 真实现（`af9e72a2`）+ 多-DMA partial-abort 修复（`6d832a38`）
- **B1** Reservation Preempt 完整 SPC-3/§6.11（`7020a2e8`）
- **B6a** MSET/FLBAS 极性修正（`6df43815`）
- **C1①** MDTS 计入 inline metadata + 适用矩阵（`0aabc8fc`）；**C1②** PRP-list chaining device→host（`c8a1bb2b`，forward scaffolding，激活待 MDTS 抬高）
- **D** persistent features(Save) + CSTS.CFS on flush 失败（`434d70b8`）
- **B6b** separate-metadata buffer（PRACT=0 host-PI via MPTR）**单 LBA WRITE+READ 闭环**：B6b-1 Format接受MSET=0(`1950f033`) / B6b-2 WRITE(`90536c52`) / B6b-3 READ(`04c76b22`）

**绿基线** `04c76b22` = 149 lib test。**B6b-4 多 LBA separate 已完成**（`8d87e7812`），并经 #4c-a/#4c-b 全 PI 路径全 tier PRP1 偏移补全（见 §1.8）。剩余功能缺口（Reservation Report EDS=1+ptpls / Security/Virt 真实现 / SGL 更多 sub_type / Write Uncorrectable）见 `plans/2026-06-11-b6b4-and-resume-plan.md` §3 + `docs/TEST_COVERAGE_PROGRESS.md`。

### 1.4 firmware spec 缺口补全 + scaffolding-SC 全收

2026-06-11（并发会话），在 `nvme_firmware` 补 firmware spec 错误处理缺口（用 openhcl e2e 挖出的）：

- **队列管理错误处理**（`a928f7ba`）：Create/Delete IO Queue 拒非法操作 — dup qid → `INVALID_QUEUE_IDENTIFIER(0x101)` / Create SQ 绑不存在 CQID → `COMPLETION_QUEUE_INVALID(0x100)` / Delete 不存在 qid → `INVALID_QUEUE_IDENTIFIER` / Delete CQ 有关联 SQ → `INVALID_QUEUE_DELETION(0x10c)`。inline nvme_spec anchor 当场抓住手填的 `0x108`(实为 INVALID_INTERRUPT_VECTOR)应是 `0x10c`。
- **abort-in-flight e2e**（`b7eed0ec`）：补 e2e lane 第一个有牙 Abort 测试。**新增 opt-in DMA-hold 机制**（`Cmd::HoldDma/ReleaseDma`，默认惰性）：hold IO write 的 data DMA → 命令滞留 `pending_ios` in-flight → Abort 命中 → 双 oracle（dw0=0 + 目标 CQE `COMMAND_ABORT_REQUESTED 0x07`）。**此 hold 机制可复用于任何「命令卡在 DMA 在飞中」的 e2e 场景。**
- **CMB-SGL 精确 SC**（`c0cfb419`）：SGL sub_type=1(CMB-relative Offset)在无 CMB 的本 controller → `SGL_INVALID_USE_OF_CMB(0x12)`（此前粗粒度 0x11）。

**✅ scaffolding-SC 4 项全收**（自包含 plan `19540e5d` = `usnvmemu/crates/nvme_firmware/docs/plans/2026-06-11-scaffolding-sc-spec-completeness.md`）：
- **item-0**（`fe30b715`）：抽 `sgl.rs::subtype_to_sc` 共享 classifier，io.rs 两路径接入 → CMB-relative sub_type=1 三路径都精确返 0x12（+2 e2e + revert-verify）。
- **item-1**（`488ffaa8`）：NS-not-ready emit `NAMESPACE_NOT_READY 0x82`。per-NS `not_ready` 字段 + 追加 setter `set_namespaces_not_ready`（不改 open() 签名）+ CLI `--not-ready-nsid` + io.rs IO 门 + **dispatch_fused_compare_write 同门（reviewer HIGH-1：fused 绕过 dispatch_io）** + Format-as-readiness + CNS 0x08 NSTAT.NRDY 跟随。
- **item-2**（`5b8dafa1`）：**spec 分析结论 = 可 emit，非纯 scaffolding**。普通 Write>AWUN spec 不 reject（确认别 fabricate）；但 fused C&W > ACWU/NACWU → spec MAY abort `ATOMIC_WRITE_UNIT_EXCEEDED 0x14`。抽 `mod.rs::fused_cw_reject_sc` 共享 classifier，本地+fabric 两 sibling 路径接入。
- **item-3**（`4c40d065`）：boot-partition **已实现**（用更简单的「只读出厂镜像」模型，非 BP-0/1/2/3 全套）：`--boot-partition-file` 装只读 BP → 广告 BPSZ>0 + 服务 Boot Partition Read + FW Commit BPID→`BOOT_PARTITION_WRITE_PROHIBITED 0x11e`。BP-2 真写未做（写尝试即拒）。

权威状态表 `usnvmemu/crates/nvme_firmware/docs/SPEC_CONFORMANCE.md`（**新规矩：改 firmware 特性随手同步该表**）。

**带注解的共享树实战教训（本次）**：另一会话整场并行做 PRCHK/PI 特性，反复占用 io/mod/completion/tests/pi.rs，逐 hunk 纠缠。最终用 `git add -p` 逐 hunk 暂存自己的 + `git diff --cached` 验证无外来内容 成功隔离提交。`git reset` 被权限拒 → 用 `git restore --staged`。验 my-only 编译曾想用 worktree 但缺 protoc → 靠 patch 干净 apply + 正交性 + 组合树测试通过 替代。协作纪律见 auto-memory `git-commit-shared-index-multisession`。

### 1.5 DBBUF（shadow doorbell）从 stub 到真实现

2026-06-11，commit `f35e5a71`。`nvme_firmware` DBBUF（spec §5.7/§7.13）此前是 stub（admin handler 只**存** shadow/event_idx GPA 不 poll + cmd.rs 广告 OACS.DBBUF=0 规避，即 vfio-user-real-guest-io bug #4 的临时修法）；现真实现：controller 真 DMA-poll driver shadow doorbell buffer 拿真 SQ tail/CQ head + DMA-写回 event_idx，OACS 重广告 true。因 `Transport::dma_read` 是 **token-异步**（无同步读，完成经 `on_dma_complete`），shadow 轮询是**跨完成回调的异步状态机**。admin qid 0 不用 DBBUF（对齐 Linux）。

**带注解的核心教训：3 层 HIGH bug 全在 async 路径、连真-guest happy-path harness 也看不见**（vfio 同步 drain 把整链跑完，故 happy-path 永远不触发边角），全靠 deterministic 单测 reproduce interleaving + revert-verify 锁住：
1. **dropped-ring strand**：poll 链 drain 时到的 doorbell ring 被 inflight guard 丢弃，若 submit 落在链最后一次 re-read 之后 → 命令滞留到 tick。修：per-(qid,is_cq) one-shot `shadow_ring_pending` flag，settle 分支若置则再 re-read 一次。
2. **wrap-saturation false-CFS**：用绝对环索引做 high-water-mark，max-depth 队列饱和于 size-1 → 健康 controller 合法 wrap 被误判无进展 → 撞 CFS。废弃绝对索引。
3. **vfio 同步 `dma_read` 让 poll 链自馈 `drain_dma_completions` while 循环**：in-range 振荡 shadow 无限卡 host 线程（guest-触发 DoS）。修：`MAX_SHADOW_POLL_ITERS` 从「无进展计数」改成**每链自续深度**上限，撞顶 CFS+收链。

**cap 度量教训（可迁移）**：cap 必须界定对的量——界定 *advance*（永远是合法进展）误伤真高吞吐；界定 *ring 距离* 是脆弱启发；自馈 async poll 的对的量是**自续深度**（host-liveness 界）。真 QEMU 11 vfio 2-vCPU 并发验过。详 LESSONS §29。

**OpenHCL async-wire e2e 覆盖**（commit `5dd5ce69`）：`openhcl_pcie_remote_e2e.rs` 加 2 测试首次在真异步 wire 行使 shadow-poll（Test 1 写 shadow=3 却敲 MMIO=1 = 分歧本身做独立 oracle + Test 2 流水线 burst timing oracle）。**测试架构洞见（可迁移）**：harness 的 biased device-first pump 让 HIGH-1 settle-窗口 race 结构性不可达 → 该精确交错由 controller 单测拥有，e2e 打更广追平路径。**单测（精确交错）与 e2e（真 wire 集成）互补非冗余**。**多队列扩展**（commit `99902ec1`）：加 `multi_io_queue_msix_vector_routing`（中断向量号做独立 oracle）+ `dbbuf_per_queue_shadow_keying`（mis-key 会发散）2 测，关闭「DBBUF 只单 IO 队列验过」缺口。20/20 e2e + reviewer APPROVE。

### 1.6 纯-4K LBAF 数据路径（firmware + fabric + 真 nvme-cli 实测）

2026-06-09 完成纯-4K（LBAF[2], lbads=12/4096B）真数据路径，混合 512B+4K 多 NS 可用。firmware 数据路径 transport-agnostic 故三 transport 都支持，但**只 firmware + NVMe-oF TCP fabric 有 4K e2e 实测**。之前 4K 只被 advertise + Format 接受，IO 路径 guard 直接拒。

- `3668b36a` firmware：READ/WRITE/COMPARE/VERIFY/WRITE_ZEROES/COPY 全 opcode + 全 PRP 档 + fused C&W 的 LBA↔byte 换算从硬编码 512 改 `sector_bytes = 1<<ns.lbads`；新增 `is_plain` 放行 4K。
- `73429e28` NVMe-oF TCP：session 合成假 PRP 原写死 512B；新增 `ns_lbads(nsid)` + dispatch_plan 扇区感知；async+sync session 每 IO 查扇区。**多 conn Format/IO TOCTOU 防护**：dispatch 同锁内复读 lbads 算 prp2 + 守 ≤2 页，超则中止 retryable 0x18。`--allow-format` opt-in 解封 Format(0x80)，0x0D NS-Mgmt 恒 block。

**带注解的教训**：① 改对称性 bug（completion 改了 dispatch 没改）reviewer 抓到 fused C&W 512-vs-4096 不一致——sweep 要覆盖所有 reachable sibling。② 自写两端的测试必须 revert-verify 能 catch bug 才算锁住。③ sync session 测试 `sess_pumps` 必须 = setup PDU 数(4) + IO 操作数，多了 server 线程 join() 挂死。

**额外捞出的真 bug**（commit `aae0eb7f`）：写独立 Python wire 测试（`scripts/interop_py/pure_4k_e2e.py`）时把 pattern 从 period-256 uniform 换成 per-LBA distinct marker + 加单-LBA 读独立 oracle，立刻捞出 **pre-existing 多 chunk fabric IO 偏移 corruption**：session 拆 chunk 后每 chunk 的 R2T 偏移/C2HData DATAO 重置为 0 而非 host-buffer 累计 → 多 chunk 读写错位。所有既有测试用 uniform pattern 静默掩盖。修=累计 `host_buf_offset`。详 LESSONS §23。

**真 nvme-cli 互通实测达成**（commit `bd100f7d`，用户跑）：`scripts/wsl_4k_format_interop.sh` 在 WSL2 kernel 6.6.114 + nvme-cli 2.8 上 **PASS** —— `nvme format --lbaf=2` → `id-ns` 确认 4096 + 8×4K distinct-block round-trip cmp 全等 + **直读 backing file 独立 oracle 确认数据落在 5\*4096 而非 5\*512**。关键设计：纯 round-trip cmp 测不出「读写都用错 ×512」自洽 bug → 必须直查 kernel 看不到的 backing file 物理偏移。RUNBOOK §0。**仍未做**：OpenHCL vsock 路径的 4K e2e。

### 1.7 Fused Compare-and-Write over NVMe-oF TCP fabric — 真原子 CAS

2026-06-09 commit `79daadc1`：fabric 接入 fused C&W 真原子 CAS。修真 bug——fabric 走 `dispatch_io` 无视 fuse bits → Compare/Write 当独立命令 → Compare 失败 Write 仍写（非原子）。

**hoisted 设计（2 轮 reviewer 后定型）**：controller 新增**无状态**原子入口 `nvme_fused_cas(...)`：单 `&mut self` 内（无 await/锁释放）read backing→比→相等才写→双 CQE。session 独占 fuse 状态：FIRST 暂存不响应；SECOND 先经 R2T 把两 host buffer 各按 cccid 取齐 → 单锁调 nvme_fused_cas → 双 CapsuleResp。

**带注解的错误（为什么 hoisted 而非首版 capture-based）**：首版给 Compare/Write 数据设不同 PRP sentinel 靠 gpa 反推 CID + 双状态，被 reviewer BLOCK：HIGH-1 两 FIRST 双状态失步→错 buffer 比→silent corruption；HIGH-2 锁中途释放非原子；HIGH-3 多 conn 共享 controller pending_fused 串对。hoisted 一举消三 HIGH 且更短。详 LESSONS §24。

### 1.8 #4 PRP1 非页对齐布局 —— 全 PI 路径全 tier 偏移支持（#4 → #4c-a → #4c-b）

`nvme_firmware` controller 支持 PRP1 任意页内偏移 O（spec NVMe Base §4.1.1：仅 PRP1 可带页内偏移，PRP2/list-entry 须页对齐；传输 >2 页时 PRP2 是 PRP-list 指针）。分三波：

- **#4（part1）+ #4b/#4f**：plain 数据路径 PRP1 非页对齐 + 中心化 `prp` helper（`202e78fc` / `563b723c`）；`prp.rs` 几何内核 `prp1_offset / first_seg_len / total_pages / page_size / tier`，O=0≡legacy 守 `offset_zero_matches_legacy`。
- **#4c-a**：inline nlb≥2（`c48b3f09`）+ separate N>2（`f14ba3c97`）PI 路径偏移——PrpListOp + reassemble-then-resplit，恒 List 档。
- **#4c-b（统一段抽象，2026-06-13/14，全 ecc:rust-reviewer APPROVE 0 C/H/M）**：P0 段抽象地基（`prp2_role`/`validate_prp2`/`dispatch_segs`，`54f18713d`）→ P1 plain 接入 `validate_prp2`（`6a18831`）→ P2 PI finalize 收敛单 `pi` 字段（`6f0119ef`/`c2c5585`）→ **P3 消灭 nlb≤2 bespoke dual 的 4096/8 硬编码**：P3① separate nlb=2 tier 分流（`31ea153`）/ P3② separate nlb=1 per-host-segment 拼回-重切（`3e28ee8`）/ P3③ inline nlb=1 Dual split 左移 (4096−O,8+O) + O>4088 升 List（`823f3fe`）→ 收尾清 P0 scaffolding（`19bd047`）。

**成果**：全 PI 路径（plain / separate nlb=1/2/N>2 / inline nlb=1/≥2）全 tier（Single/Dual/List）PRP1 偏移 O∈[0,4095] 零 spec-legal 例外。P3 各修真实**数据完整性洞**（旧 bespoke O>0 盲读跨页 + 漏段），非纯重构。**关键技术点**：物理页边界 ≠ 逻辑单元边界 → 拼回连续流再按逻辑大小重切；inline 的"页划分 (4096−O,8+O)"与"data/tuple 划分 (4096,8)"是**两条正交切轴**，finalize 在拼回后的完整 block 上切 data/tuple，绝不混淆。差分 oracle 四件套（含显式断每条 DmaRead/Write 段长 + 断 tier 分支 + revert-verify 两向）。设计/进度见 `crates/nvme_firmware/docs/plans/2026-06-13-4cb-unified-prp-segment-abstraction.md`，状态见 `SPEC_CONFORMANCE.md` PI 行。

---

## 2. Transport e2e harness 里程碑

> 三 transport（OpenHCL pcie_remote / vfio-user / NVMe-oF TCP）对 firmware 核心数据路径**均已有跨进程真-firmware e2e harness**，且都达到 L3 真 guest/host e2e。

### 2.1 三 transport 成熟度不均衡 → 已解决（历史曲线）

2026-06-09 用户主动点出不均衡（git 证据：近 30 天 commit `nvme_of_tcp_target` 16 / `vfio_user_transport` 14 / `nvme_firmware` 14 vs OpenHCL VTL2 device 半边自 05-30 冻结）。**根因（带注解的反思）**：「纯代码可无人值守 backlog」恰集中在 nvme-of → 自主优化成了「我能独立做什么」而非「什么对 vision 最核心」；而用户**当时**自陈主战场 OpenHCL 反被冷落。

> **注（2026-06-12 用户更正）**：「OpenHCL 是主战场」是**当时**的框定，后被作废——**实际无单一主战场，各接入平等、按价值挑活**（见 auto-memory `scope-usnvmemu-always`）。下面的不均衡诊断与「便宜 harness PASS = 结构性假安心」教训仍成立。

**结论（仍有效的判断）**：firmware-as-core 地基健康（core 成熟 + `trait Transport`/hexagonal 边界干净，新 firmware 特性自动流向各 transport）；曾经的不均衡（nvme-of 过度 > vfio 刚 parity > OpenHCL 冻结）**2026-06-10 已彻底解决**：
- OpenHCL pcie_remote 从「只有 noop 烟雾测试」拉到跨进程真-firmware e2e（§2.2/§2.3）。
- vfio-user 真 QEMU 11 guest 真 IO（§2.5）。
- nvme-of 早有真 nvme-cli。

**可迁移教训**：便宜 harness（小内存 / realize-only / loopback / memfd）的 PASS 是**结构性假安心**——LESSONS §26（小内存 harness 看不见 >4 GiB）+ §28（realize-only 掩盖真 guest 整类 bug）。别再当「OpenHCL/vfio 冷落」翻旧账。

### 2.2 OpenHCL pcie_remote 跨进程真-firmware e2e harness（Linux 可测）

2026-06-10 新增 `nvme_firmware/tests/openhcl_pcie_remote_e2e.rs`：pcie_remote transport（firmware-as-core 第 1 条接入）上第一个驱动真 NVMe firmware 的跨进程 e2e harness。**Linux 可测、无需 Windows/Hyper-V**——harness LISTEN TCP，spawn 真 `nvme_firmware --tcp-addr` bin（device 侧 = TCP client），harness 扮 OpenHCL/VTL2 侧（发 Hello、收 HelloAck、发 MMIO/doorbell、按 16MiB flat guest-mem 服务 ReadGpa/WriteGpa DMA、收 InterruptFire）。

- **4 增量**（commits `94c5b741`→`99ca1cf2`）：O1 握手 + NVMe 身份 / O2 admin queue 全路径(CC.EN→CSTS.RDY + Identify + MSI-X) / O3a 纯-4K Format+IO round-trip + 直读 backing file 独立 oracle + CQ phase-wrap / O3b fused C&W 原子 CAS。
- **关键设计（reviewer 定）**：pcie_remote DMA 是异步 token fire-and-forget，`codec::read_frame` cancel-unsafe → 专职 read-task 喂 channel + pump select 两个 cancel-safe channel。4 轮 rust-reviewer 把两 oracle 验到 airtight，0 C/H/M。
- **P1 admin-PRP 补全也用本 harness 测**（commit `5a87cf08`）：第 5 个 test `openhcl_get_log_page_noncontiguous_prp` 抓真 bug——`dma_write_then_complete` 原把整 buf 连续写单 prp1、无视 PRP2（8 call site），> 4 KiB 在非连续 PRP 真 host 上 silent 写错址。修=复用 IO read dual-PRP 机件，revert-verified。
- **P2 ✅**（commit `2a63a6d5` + followup `cb48faee`）：>2 page 走真 PRP list（复用 IO read 的 `PrpListOp`）。Get Log Page 上限 8KiB→2MiB。reviewer 把 513 页边界对 NVMe spec + 真 OpenVMM `make_prp` host driver 双验 byte-compat。**admin/IO 数据 PRP 通路 spec-complete**。

仍 defer = L3 真 Windows OpenHCL VM guest driver（须用户+Hyper-V）——见 §2.3。

### 2.3 OpenHCL L3 真 Hyper-V guest e2e 重验 + committed harness（抓到 ASQ/ACQ 截断 bug）

2026-06-10：pcie_remote transport 的真 Hyper-V guest e2e 用当前 `nvme_firmware` binary 重验通过，并把 ad-hoc flow 固化成 committed harness。

- **实证**：guest WS2025 枚举 `OpenHCL Userspace NVMe v2.0` → format NTFS → 写+回读 4 MiB(`markerMatch=True`)→ controller log 见 PRP-list 路径 → **独立 host oracle**：guest 写的 marker 在 raw backing file 字节偏移 **22577152** 找到。
- **带注解的最大收获——harness 当场抓到真 firmware bug**（commit `b68da9dc`）：`controller/mmio.rs` 对 ASQ(0x28)/ACQ(0x30)/BPMBL(0x48) 三个 8-byte 寄存器的 MMIO 写只按 offset 匹配、无视 size，单条 8-byte(qword)写被 `value & 0xffff_ffff` 截断成 32 位。Windows nvme.sys 单条 qword 写 ASQ；guest(≥4 GiB RAM)把 admin 队列放 4 GiB 以上时高 32 位丢 → SQE fetch 读错 GPA → 首条 admin 命令读成全 0(opc=0x0) → guest init 挂（**间歇**）。三个 in-process/小内存 harness GPA 永远 < 4 GiB，**结构上命不到**——真 host e2e 不冗余。fix=size-aware + inline 单测 + revert-verify。详 LESSONS §26。
- **committed harness**（commit `3081949c`）：`usnvmemu/crates/nvme_firmware/scripts/hyperv_interop/`（对标 vfio `qemu_interop` / nvme-of `interop_py`）：`run_e2e.sh`(WSL 入口) + `run_hyperv_nvme_e2e.ps1`(deploy-or-reuse VM + guest IO oracle) + `build_and_stage.sh`。双 oracle（guest readback + ps1 内 controller-alive raw 字节扫描）须一致才 PASS。
- **环境**：VM `pcie-remote-exp` GUID `fb36a84c-256b-44c2-98d8-0d47345cf092`，IGVM `openhcl-pcie-v14-8s.bin`(GuestFeatureSet 0x201)，guest cred Administrator/PcieRemote123!，staging `/mnt/c/temp/pcie_remote_exp/`。
- **Windows 交叉编 gotcha**（下次直接照做）：`nvme_firmware` 在 root Cargo.toml `exclude` → 必须 cd 进 crate dir 独立编（不能 `-p`）；`vfio-user`(default feature) Unix-only → Windows 编必须 `--no-default-features --features openhcl`；`build-windows-cross.sh` ROOT 算法 2026-06-08 crate 搬家后 off-by-one(已修)；worktree 编需 symlink 主仓 `.packages`(protoc 等)。
- **2026-06-11 post-DBBUF 复验 PASS**（commit `e72a3d28`）：含 DBBUF 的 binary 复跑 L3 仍 PASS。**Windows nvme.sys 不用 DBBUF**——L3 验的是非-DBBUF MMIO 路径无回归；DBBUF 由 Linux 侧 e2e 覆盖。**配置：默认复用单一 VM**（`FORCE_DEPLOY=0`=reuse；每次跑只重置 backing img + 重启 host controller(`nvme_firmware.exe --vm-id`，AF_HYPERV vsock，跑在宿主非 VM 内) + `Stop-VM`→`Start-VM` 冷启重握手）。

### 2.4 vfio-user 真 QEMU 11 e2e（realize + PCI 枚举）

2026-06-09 里程碑：vfio-user transport（第 3 条接入）用**真 QEMU 11.0.1** 做 e2e 验证通过。

- **带注解的关键事实纠错**：vfio-user 客户端（`vfio-user-pci` 设备）**已在 QEMU 10.1（2025-08，Nutanix/John Levon）合入上游主线**，不是「需 fork」。早先基于 QEMU 8.2 写的「上游无 client」结论是错的（LESSONS §21 — version snapshot ≠ upstream capability）。linuxbrew QEMU 11 即可。
- harness 在 `usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio.py`（commit `b7d5adc4`）。
- **三层 oracle 独立性递增**：自家 Python client → libvfio-user 官方 C client → 真 QEMU 11。
- **带注解的 2 个自家测试全绿的握手 bug**（commit `f8fdf220`）：①version minor 须 `min(client,server)`（QEMU 提议 0，硬编码回 1 被判 incompatible）②max_msg_fds 须 ≤ QEMU `VFIO_USER_MAX_MAX_FDS=16`（原广告 128 → malformed）。印证：第三方实现才能 catch self-consistent bug（auto-memory `review-not-optional-self-consistent-trap`）。
- **vfio spec-complete track 全部完成**（7 commits `f8fdf220`→`51ab5374`）：握手协商 + bulk REGION_WRITE + REGION 上限改 server 广告 max_data_xfer_size + describe() 懒缓存 + **mmap 零拷贝 DMA**（DMA_MAP memfd 直映射；2 轮 reviewer 首轮 BLOCK 抓 C-1 SIGBUS=client size>fd 真实大小，修=fstat 独立 oracle，LESSONS §22）。
- **build 注意**：usnvmemu 各 crate 独立、**非 workspace**，须 `cd crates/nvme_firmware && cargo build --features vfio-user`，不能用 `cargo -p`。
- 当时边界：`-S` 暂停 CPU，只验到 device realize + PCI 枚举（query-pci 见 0x1414/0xc0de）。真 guest IO 见 §2.5。

### 2.5 vfio-user 真 guest NVMe IO + guest-boot harness（realize-only 掩盖的 4 个 server bug）

2026-06-10：vfio-user transport 从「realize-only」升到**真 guest 做真 NVMe IO**。建了 guest-boot harness `crates/vfio_user_transport/scripts/qemu_interop/run_qemu_vfio_guest.py`（真 Ubuntu 6.8 bzImage + busybox initramfs + 真 nvme 驱动 + host backing 裸读独立 oracle）。

**带注解的 4 个 realize-only（QEMU `-S` 暂停 + 只读 config space）结构性命不到的 server bug**：
1. posted(NO_REPLY) write 回了 reply → **wire 失步**（QEMU 把那 16 字节 reply 当下条消息 header 解析）。
2. config space 缺 MSI-X capability → guest `nvme_probe -EINVAL`。
3. SET_IRQS 拒绝 masked vector(0 fd，旧码 `fds.len()!=count`→EINVAL)。
4. 广告 DBBUF 但只存 shadow 不轮询 → Linux 跳过真 MMIO doorbell → IO 30s 挂（当时临时改广告 false；**2026-06-11 已真实现 DBBUF 轮询**，见 §1.5）。
+ `handle_region_write` 用 NoopTransport 丢了 MMIO-write 内设备发起的 DMA(admin SQE fetch)。
+ OpenHCL 回归(reviewer MEDIUM)：`describe()` 的 MSI-X cap 转发给 OpenVMM(自己用 MsixEmulator 合成)→双 cap/握手拒；修在 `pcie_device_sdk/run.rs` 转发时 strip(cap_id 0x11)。

commit fix `0bc4dbe2` + harness `c885aba6`；真 QEMU 11 guest 枚举 /dev/nvme0n1 + 4MiB IO + 双 oracle GREEN；16/16 `openhcl_pcie_remote_e2e` 不退；rust-reviewer 2 轮。LESSONS §28。**guest kernel gotcha**：WSL2 自带 kernel 是 raw vmlinux ELF，QEMU `-kernel` 报 `without PVH ELF Note` 不收；`fetch_guest_kernel.sh` 从 Ubuntu 包提取 6.8 bzImage + nvme.ko（无 sudo）；busybox 自带 cpio applet。用户本地 QEMU master fork 在 `/home/xp/src/qemu-fork`。

### 2.6 真机实测：VTL2 独立进程零拷贝直访 guest RAM（topology A 可行性）

2026-06-11 真机 POC（活 OpenHCL VM `pcie-remote-exp`）：一个**非-underhill 的独立 VTL2 进程**（root）能 open `/dev/mshv_vtl_low` + 线性 mmap guest RAM（`file_offset = 裸 GPA`，非-CVM 此 VM 无需 SHARED_MEMORY_FLAG）+ 真读到 guest 数据 + 写回一致。

⟹ **topology A（firmware-in-VTL2 零拷贝）真机证实可行**：把 Linux firmware binary 放进 VTL2，即可直访 guest RAM 做零拷贝 DMA，不必走 pcie_remote 的 ReadGpa/WriteGpa 转发。这是「vfio-user 概念落到 Windows/OpenHCL」的可行零拷贝路径。

- **关键实测细节**：`/dev/mshv_vtl_low` root-only；设备**只支持 mmap，不支持 read()**（dd 报错）。机制本就被 `openhcl/underhill_mem/src/mapping.rs` 用。
- **可复用技巧——免重建 IGVM 在活 VTL2 跑任意静态 bin**：`ohcldiag-dev run` 转发 stdin，VTL2 有 `/bin/base64` + 可写 `/tmp`：
  ```bash
  rustc --edition 2024 -O --target x86_64-unknown-linux-musl -C target-feature=+crt-static probe.rs -o probe && strip probe
  base64 -w0 probe | /mnt/c/temp/pcie_remote_exp/ohcldiag-dev.exe <vm> run /bin/sh -- -c 'base64 -d >/tmp/p; chmod +x /tmp/p; /tmp/p'
  ```
- POC 套件在 `usnvmemu/experiments/2026-06-11-openhcl-vfio-user-client-poc/`。

---

## 3. vfio-user-in-underhill（W0 → W6c saga）

> 把 vfio-user「客户端」概念真正落进 underhill（VTL2），让 guest 在真 OpenHCL VM 里枚举并驱动一个由 VTL2 内 usnvmemu 进程提供的 PCIe NVMe 设备。各 W phase 详细 plan 在 `docs/superpowers/plans/2026-06-1x-w*.md`，spec 在 `docs/superpowers/specs/`，实验归档在 `usnvmemu/experiments/2026-06-1x-*/`。

### 3.1 W0–W5a：sans-IO wire crate + 客户端 + 真 VM 零拷贝（历史，已 shipped）

- **W0**（squash 单 commit `1213e951`，plan v2 `e3495aa0`）：新 crate `usnvmemu/crates/vfio_user_wire/`（仅 zerocopy + thiserror + pcie_device_core，零 nix/tokio/memmap2/libc）。搬 proto/access/config + hand-edit handshake.rs 抽 pure 决策（negotiate_minor / parse_caps_blob / build_version_reply_payload）。transport 端用通配 re-export shim 零改动。wire 31 + transport 69 = 100 test。
- **W0.5 cleanup**（单 commit `48a4fd17`）：Track A `framing::Message` 拆 sans-IO `WireMessage` + transport `Message{wire,fds}` Deref；Track B `irq::decide_set_irqs_action`；Track C `NoopTransport` 迁 `pcie_device_core` 改 **panic 桩**（非 silent——曾有 NoopTransport 误接 IO 致 silent-drop doorbell DMA 史）。wire crate 现 6 模块全 sans-IO。
- **W1**（单 commit `920795a2`）：新 `vfio_user_device` crate（`#![deny(unsafe_code)]` 无 nix——握手无 fd 用纯 UnixStream）。wire crate 加 client 纯决策 `build_version_command_payload` + `verify_server_version_reply`（major 相等 + minor ≤ 提议，方向与 negotiate_minor 相反防 server 回更高 minor）。**带注解**：packed Header/VersionPayload 字段读取必须先 copy 到本地 let（`#[repr(C,packed)]` + deny(unsafe_code) → 直接读是 E0793）。
- **W2**（单 commit `bfd45834`）：**scope ADR（用户拍板选项3）**：W2 = client REGION wire only；**guest-facing PCIe 呈现（依赖重量级 pci_core）推迟 W6**。理由：pci_core 是 workspace member（拖 chipset_device/guestmem/vmcore/mesh 全家桶），path-dep 进 exclude crate 破坏 standalone Linux 可测。client `get_device_info`/`get_region_info`/`region_read/write`/`reset`；loopback 集成测对接真 `VfioUserSession`+`MockDev`。
- **W3**（单 commit `5bc80ea8`）：DMA_MAP 传 GuestMemory region fd。**带注解的重大利好**：发 fd 是**全 safe nix API**（sendmsg + ControlMessage::ScmRights），**保持 #![deny(unsafe_code)]**——收 fd 才需 unsafe，client 只发不收。**调试驱动修 2 真问题**：(1) GPA_DST 验证时序（on_dma_complete 的 dma_write 在 reply 之后）；(2) on_dma_complete 无限递归（Transport::dma_write 本身也 push 完成事件，无条件 echo→死循环，用 echoed bool 守门）。
- **W4**（单 commit `a02a33cd`）：MSI-X eventfd → client `set_irqs`（EventFd→OwnedFd 全 safe nix）。loopback 对标 POC-2：client 建 4 EventFd → server 写 client eventfd（同内核对象 dup）→read 验计数。**W6 是汇合点**：W2(PCIe 呈现)/W3(JIT)/W4(Interrupt::deliver) 全把「需 underhill partition/openvmm」半段推迟 W6。
- **W5a 档2**（单 commit `6a633c4b`，**真机 e2e PASS**）：真 nvme_firmware ELF（cross-build static-pie musl，零改）在真 VTL2 跑 + test client 经 ohcldiag-dev run+base64 stdin 推进 → 握手 → dma_map 真 guest RAM(/dev/mshv_vtl_low) → firmware 零拷贝 DMA Identify → client 读回 MN「OpenHCL Userspace NVMe v2.0」。POC-1/3/6 + W1-W4 真机首次汇合（topology A 端到端）。
  - **带注解的 W3 真 bug（real-VM surfaced，违反 poc-before-settling 教训自纠）**：`vfio_user_transport::map_dma_fd` 的 fstat st_size 上界（防 SIGBUS，对 memfd 有意义）误拒**字符设备** mshv_vtl_low（st_size=0）→ 零拷贝失效。**W3 只用 memfd 测漏了真目标（真 guest RAM fd 永远是字符设备）→ 零拷贝在真目标上从未真工作过**。修：仅 S_IFREG 施 st_size 上界，字符/块设备跳过。**教训：loopback/memfd 测过 ≠ 真目标过。**
  - committed harness `experiments/2026-06-12-openhcl-vtl2-deploy/`。scope（用户拍板档2）：W5a = dev 部署 + 真 guest RAM 零拷贝真 NVMe IO；W5b(underhill supervisor)≡W6 推迟；W5c(IGVM 生产) 远期。

### 3.2 W6a：client async 化（历史，已 shipped）

W6a（单 commit `571ad805`）：

- **用户拍板**：放弃「不惜代价保 deny(unsafe_code)」，用成熟高性能库 + 容许 unsafe 在高性能部分。
- **照搬 gold standard** `vm/devices/virtio/vhost_user_protocol/src/socket.rs` 混合范式：pal_async `PolledSocket` 管 readiness（锁不跨 await）+ 裸 libc `sendmsg/recvmsg`+`cmsghdr` 管 SCM_RIGHTS。**client 只发 fd 不收 fd**，比 vhost_user 简单（recv 只收字节 + drop 意外 fd 防泄漏）。
- 新 `async_socket.rs`（**唯一含 unsafe 的模块**，`#![allow(unsafe_code)]` 每 block SAFETY）；其余模块级 `#![deny(unsafe_code)]`。nix 从 prod 降 dev-dep。deps 加 `pal_async` + `libc` + `parking_lot`。
- oracle：lib 7 + loopback 4 = 11 PASS；transport 68 / nvme_of 172 不退化；harness host + musl 静态构建均通过。
- **W6b 前瞻注记**（lib.rs）：当前 `Mutex<PolledSocket>`（request/reply 串行足够）；若 W6b 需 server-initiated 与 client request 并发，改 `PolledSocket::split()` 读写半（YAGNI 不预置）。
- **关键设计教训**：plan 里别为保 deny 自创 nix-hybrid 变体；照搬同仓库维护者熟悉的成熟 pattern 才是长远正确（auto-memory `adapt-proven-component-preserve-premises`）。

### 3.3 W6b：underhill ChipsetDevice 设备 + reconnect + 真 guest 枚举（PROVEN）

**W6b plan** `docs/superpowers/plans/2026-06-12-w6b-vfio-user-pci-device-underhill.md`（architect-reviewed）。**scope 经用户纠正**（auto-memory `meaningful-complete-not-minimal`）：W6b = 完整功能设备（guest 枚举**并驱动**真 IO），不是「出现在 lspci 就完」。**architect 2 BLOCKING**：①**B-1**：worker 必须**全双工**（`PolledSocket::split()` writer+reader + in_flight `HashMap<msg_id>`），**不能**串行 `client.region_read().await`（阻塞整个 select loop = 真死锁）；②**B-2**：member path-dep exclude crate 标准可行。

- **W6b Phase 1**（coarse commit `733459d0`，23 files +3090，subagent-driven 7 task）：新 crate `vm/devices/pci/vfio_user_pci_device`（workspace member，path-dep exclude crate vfio_user_device/wire）+ `vfio_user_pci_resources`。全 standalone：device.rs（ChipsetDevice shim，cfg/MSI-X 本地 + 其它 BAR MMIO defer→worker）/ worker.rs（**B-1 全双工** writer+reader split + in_flight HashMap<msg_id>，单 wire Err→Lost）/ irq.rs（pal_event::Event eventfd → PolledWait → Interrupt::deliver）/ spawn.rs / resolver.rs（**H-4 set_irqs timeout 兜底 AbsentPcieDevice** + H-3 async assemble + M-1 BAR0 自动 64-bit）。vfio_user_device 加 `into_channel()` 全双工 split。新 crate 15 lib + 4 integration；vfio_user_device 13 测不退化；clippy 0。
- **W6b Phase 2**（coarse commit `0fb8e8b0`，7 files +168 纯增量）：underhill_core 集成（照抄 pcie_remote pattern）。options.rs（`VfioUserNvmeCliConfig <guid>:<unix_path>` + OPENHCL_VFIO_USER_NVME env）/lib.rs 桥接/worker.rs 注册块（CVM gate + boot grace poll + env-gate）/vtl2_settings_worker.rs。**env-gated+CVM-gated+servicing-gated → 未设 env 零行为变化，真 VM 验证前可安全提交**。
- **Phase 2 Task 2.4 Step 2 realize-only 真机 PASS**（commit `57632f6d`）：IGVM 装真 OpenHCL VM + 设 env 但不起 firmware → connect retry×32→cap→`boot grace done got=0`→AbsentPcieDevice 兜底→**underhill boot 不挂**。

> **⚠️ 带注解的错误：Phase 2 realize-only `57632f6d` 实为 false-PASS**——只查 VTL2 可达没查 control_state；见 §3.3 发现①。**真机判据必须查 `ohcldiag-dev inspect control_state`(==running)/guest 枚举**，而非周边「还活着」信号。

#### W6b reconnect Layer A（commit `8c47c4b9`，17 files +1892/-729）

完整 brainstorm→4 POC→3 轮 subagent 对抗审计→spec→plan→subagent-driven。用户要 **usnvmemu 启停灵活性**（「firmware」→统一叫 **usnvmemu**=VTL2 内 vfio-user server 进程）。设计 = operator 拥有 usnvmemu 生命周期（运行时启停/换二进制，不重建 IGVM）+ underhill 持久 reconnect（device 随 usnvmemu 生灭 Lost↔Live revive，已枚举 function 范畴）。

- **审计修正（pci_core/vpci 源码事实）**：① pci_core 构造即定（运行时不可改 BAR/identity/MSI-X）→ 首连学 identity 改活 cfg 不可能；② VPCI offer/revoke 仅 host-owned VF bus（非 emulated 设备）→ **真 guest hot-add/remove(冷插)=BLOCKED，需净新增 VpciBus plumbing=Layer C/未来**；③ pcie_remote=槽位恒在+cfg-error 表 Live/Lost+transport 重连。→ 原「3 可配置模式」砍成 **slot-always-declared 单模式**。
- **3 CRITICAL 不变量**（happy-path 看不见，§1.5 DBBUF 类）：C-1 pair-swap 原子（writer+reader 一条消息+两赋值间无 await）；C-2 show-absent-until-Live；C-3 reconnect 重发 set_irqs（持久 eventfd owned-clone，否则 revive 后 MSI-X 死）。
- 实现：reconnect_loopback **6 场景**全 PASS；20 lib+9 集成测；clippy+fmt clean。**轨迹归档**：`usnvmemu/experiments/2026-06-12-w6b-reconnect-poc/`；spec/plan 在 `docs/superpowers/`。

#### W6b reconnect 真机 e2e（commit `1e210435`，归档 `experiments/2026-06-12-w6b-reconnect-real-vm/RESULT.md`）

**核心 PROVEN** — vpci IGVM + 配 instance → device assemble(Connecting) → 连接器持久重试 → operator setsid 起 usnvmemu → connect+握手+C-3 重发 set_irqs → worker `reconnected, Live`。

- **承重发现①（必记）**：`cargo xflowey build-igvm x64` 默认**不带 vpci feature**(只 gdb,tpm)→ 配 vfio device 后 underhill `bail!("built without vpci support")` → control_state 卡 "starting"（但 VTL2 diag 可达+Hyper-V Running，「看似不挂」）。**修=`--override-openvmm-hcl-feature vpci`(零代码改)，产物 `debug/x64-custom/openhcl-x64-custom.bin`**。
- **✅ 发现②已更正＝测试假象，非 bug**（commit `5c074c57`）：VTL2 busybox `pkill -x usnvmemu` 匹配不到真进程（comm 对但 busybox `-x` 怪癖；`pgrep -f` 能找到），之前「停 usnvmemu」啥也没杀→worker 正确保持 Live。用 `pkill -f "/tmp/usnvmemu --vfio-user-sock"` 正确 kill 后：停→`going Lost`(✓)；重起→2nd `reconnected, Live`(revive ✓)。**完整 reconnect 生命周期全真机 PROVEN**（assemble→Live→Lost→revive，C-1/C-2/C-3/边沿-lost/持久重连/idle-EOF）。**教训：VTL2 busybox kill 用 `pkill -f <cmdline>`/按 pid，勿 `pkill -x <comm>`；下结论前 `pgrep -f` 确认 kill 真生效。**

#### W6b finding-③ FIXED + 真机 PROVEN（commit `bbdfd7cc` 码 + `02150223` 归档）—— 首次真 guest 枚举 W6b 设备

- **finding-③（reconnect 真机调试暴露）**：guest 把设备枚举成 `PCI\VEN_1414&DEV_0000`(Unknown/无驱动/无 disk)，即便 device 后来 Live。
- **根因（VPCI 源码追踪确认）**：VPCI offer 在 `assemble_device`(Connecting 态)一次性 latch 身份——`VpciChannel::new` 经 `probe_hardware_ids`/`probe_bar_masks` 调**设备自身 `pci_cfg_read`** 读 offset 0/8/0x2c + BAR regs，存 `hardware_ids`(之后只读不 re-probe)，随 `BUS_RELATIONS2` 发 guest（`vpci/src/device.rs:1339-1345` + `chipset_device_ext.rs:47-76`）。**旧 C-2「Connecting cfg 返 Err」让 latch 捕获 DEV_0000 → guest devnode 永久错**。这是 auto-memory `poc-before-settling-design` 里详述的「被三层防线放行、建立在未验证平台假设上的整套设计」真机翻车案例。
- **修法（C-2 修订，option-A 槽位恒在+真身份）**：device.rs `pci_cfg_read/write` **恒呈真 declared config**（含 Connecting/Lost）→ offer latch DEV_00A9+CC_010802+BAR 掩码 → guest 加载 stornvme；**MMIO 改按 Live 门控**（非 Live 返 `Err`，**绝不 `Defer`**——291d8645 hang 是 Defer 致，Err 安全）+ MSI-X BAR 本地恒服务。rust-reviewer APPROVE；cfg-write 真穿透回归测锁 `probe_bar_masks` 承重前提。
- **真机 PROVEN**：boot → push musl usnvmemu(base64-stdin) + setsid 起 → `worker: reconnected, Live` → **guest PSDirect 枚举 `DEV_00A9` + "Standard NVM Express Controller" + stornvme 绑定**（对比修复前 DEV_0000）；disable/enable 重 init 后 MMIO 命中 Live 控制器（CC.EN/CSTS/doorbell/admin-queue 全通）。
- **wait-for-start Hyper-V 瞬态**：`OPENHCL_WAIT_FOR_START=1` boot 必 `VirtualizationException`（非内存非设备）；改用 proven 配置 + early push usnvmemu 绕开。

### 3.4 W6c = finding-④（DMA 数据路径）— ✅ 真机 PROVEN（W6b 完整闭环达成）

> **W6b 至此完整达成**：guest 枚举 + 驱动设备 + 真零拷贝 IO，端到端双重独立验证完成。commit `7cd8bfa4`（码）+ `6fc6e500`（归档）+ `1bed8c4f`（oracle-2 PASS）。

**缺口**：finding-③ 修复后 MMIO 全通但控制器 DMA 失败——usnvmemu 收 **0 DMA_MAP**，读 admin SQ `gpa=0xeae2b000 "not in any DMA region"` → 无 disk。根因：W6b worker 只发 MMIO，DMA 零拷贝从未接进 underhill。

**策略 A 实现**（源码追踪 + 真机 POC 双验后选定，对比候选 B=经 handle/resolver 显式传 MshvVtlLow fd 照搬 W5a）：underhill 非隔离 VTL0 `GuestMemoryView` 实现 `GuestMemoryAccess::sharing()`，逐 `memory_layout.ram()` 段产出 `ShareableRegion`（保留 `MshvVtlLow` fd 的 `Arc<OwnedFd>`）；resolver `get_regions().await` → reconnect 在 set_irqs 后、`into_channel` 前对每段 `dma_map`，**每次重连重发**（同 C-3，挂 `reconnect.rs`）。生产参考 `vhost_user_frontend`（sharing→get_regions→ship fds）。

- **隔离安全加固（security-reviewer HIGH）**：把「恰好没 bitmap 所以安全」的隐式耦合升级为构造点**显式 `.shareable(true)`**（仅非隔离 VTL0）+ `build()` 里 `do_share = shareable && no_bitmap_gating` + `debug_assert!(no_bitmap_gating)`。CVM/隔离/bitmap-gated mapping 即便误设也结构性返 None。
- **带注解的承重事实（真机修正，可迁移教训）**：跨进程共享 guest RAM 的 `fd_offset` 要用**直接视图（裸 gpa）**，不是 host 自身的 aliased mapping offset。本 VM alias-map ON（VTL1 启用），**初版用 aliased offset + 「alias-on 禁用」门控 → DMA_MAP=0 无 disk**；改用裸 gpa（mshv_vtl_low offset=gpa 直接映射 VTL0 内存，与 guest 在 PRP/SGL 填的裸 GPA 一致）后即通。一句话：**device DMA 走 IOMMU/直接物理视图，不是 host CPU 的受保护别名视图**。
- **承重 POC①（已验）**：DMA_MAP 覆盖高 GPA（0xeae2b000≈3.67GiB；W5a 只验过 0x100000 处 64KiB）——按 `memory_layout.ram()` 每段一 region（MMIO hole 排除）。
- **PROVEN 实证**：DMA_MAP×2 zero_copy（裸 gpa = ram() 低/高段）+ dma_read fail=0 + guest disk「OpenHCL Userspace NVMe v2.0」256MB Online + **4MiB write/readback markerMatch=True（oracle-1）** + **独立 oracle-2（流式 `file -p` 导出 backing → host 扫，marker @ byte 22577152）** + dma_read OK×4233/write×1255 + revive 重发 DMA_MAP。
- **带注解的 finding-⑤（root-caused = harness artifact，非产品缺陷）**：oracle-2 初版从 VTL2 `ohcldiag-dev run grep -a` 扫 256MB backing 可复现崩 VTL2（panic-reboot）。差分确认：同文件流式 `file -p` 通、grep 崩 → 根因 = **`grep` 在无换行二进制上把整文件当「一行」缓冲 → 512MB-RAM VTL2 OOM → `oops=panic`→reboot**（触发器是读大 mmap'd 文件，非 IO 路径——IO 干净）。**规避**：从 VTL2 取大文件用流式 `file -p` 导出到 host 再扫，勿在 VTL2 内对二进制 `grep`。
- **VTL2 判活务必 `ps` 实证**：`pgrep -x <comm>`（busybox 怪癖）与 `pgrep -f <含自身的 pattern>`（匹配到自己）两种误报都踩过。
- 归档：`experiments/2026-06-12-w6b-reconnect-real-vm/RESULT.md`（finding-③④⑤）。

**W6b 之后的方向**（非 W6c）：① Layer C 真 guest hot-add/remove **已收口**（真机证 Windows 平台硬限 → 采 Option B「transient 停顿 + C-3 透明重连」，见 `experiments/2026-06-12-layer-c-c0-real-vm/RESULT.md` + memory `vfio-user-underhill-state`）；② usnvmemu 独立托管服务 **已落地**（§3.6：init env-gated autostart，零-operator 出盘真机 PASS）；以及 §2.1 所列各接入的其它落地（按价值挑，无固定主战场）。

### 3.5 IGVM 构建笔记（W6b/c 真机必备）

- **IGVM 已构建可用**（2026-06-12，`cargo xflowey build-igvm x64` 成功，产物 `flowey-out/artifacts/build-igvm/debug/x64/openhcl-x64.bin`）；配 vfio device 须加 `--override-openvmm-hcl-feature vpci`（见 finding①）。
- **集成在真 musl VTL2 target 干净编译**（比 host cargo check 严）。
- **带注解的坑**：WSL 重启打断的构建会留**损坏的 incremental cache** 致 rustc ICE（`nvme_driver` lint_mod / try_mark_green dep node，非代码 bug）；修法 `rm -rf target/openvmm_hcl/x86_64-unknown-linux-musl/debug/incremental` 后重跑即过。
- **带注解的中间结论（后被 reconnect/operator 模型取代）**：早期判断 guest 枚举要 firmware 先于 underhill connect spawner(init 早期 ~0.7-3.9s) listening，且外挂两路实测均不行（首 boot 时 VTL2 diag 未起无法注入 / ohcldiag-dev restart 重置 VTL2 /tmp 致 socket 消失），故当时认为「正确做法是 firmware 由 VTL2 init(initrd) 或 underhill supervisor 启动」。**实际 W6b 用 §3.3 的 operator-managed 生命周期 + 持久 reconnect 模型解决了枚举**（boot 后 operator setsid 推 usnvmemu → device 从 Connecting revive 到 Live → guest 枚举正确，见 finding-③ PROVEN），**不需要** supervisor/initrd。supervisor/initrd 自启动作为「usnvmemu 独立托管服务」方向**已于 §3.6 落地**（init env-gated autostart，零-operator 出盘真机 PASS）。

### 3.6 usnvmemu VTL2 自启动托管服务（W6 收尾）— ✅ 零-operator 出盘真机 PASS（commit `738ab808d`）

让 vfio-user NVMe 设备在 OpenHCL boot 自动出现，替代 operator 手动 `ohcldiag-dev push + setsid` 起 usnvmemu。三层 env-gated（未设零回归）：① usnvmemu(static musl) 烤进 IGVM initrd `/bin/usnvmemu`（新 `openhcl/usnvmemu_fs.config`；一等 flag `cargo xflowey build-igvm --with-vfio-user-nvme <musl-bin>`，或手动 `--custom-extra-rootfs`+`OPENHCL_USNVMEMU_PATH`）；② `underhill_init`(PID1) env-gated 在 spawn underhill 前 spawn usnvmemu；③ underhill_core 现有 device + 持久 reconnect（不改）。

- **配置**：`OPENHCL_VFIO_USER_NVME_AUTOSTART=<size_mb>:<backing>`（colon、**空格-free** 以过 kernel cmdline→init env 不被截断；sock 从设备 env `OPENHCL_VFIO_USER_NVME` 派生**不重复**）。init 建 backing file（usnvmemu `NvmeController::open` 无 `.create`，要文件已在）。
- **PID1 安全（architect + rust-reviewer 评审纳入）**：`pre_exec` 内 `RLIMIT_CORE=0`（usnvmemu segfault **不触发** `core_pattern=|/bin/underhill-crash`——其无 PID 过滤会向 host 流 core=**假 VTL2 崩报**；令异常退出**静默退化 boot-absent**）+ `setsid` + `dup2(2,1)`（usnvmemu `tracing` 走 stdout，dup→stderr=ttyprintk→kmsg，否则继承 init `/dev/null` 静默丢日志）；CVM-gate（隔离 VM 不 autostart）；错误**绝不**逃出 `do_main`（逃出→`main` `exit(1)`→PID1 死→kernel panic）→ `if let Err` 吞掉。**不加 crash-restart**（VTL2「非预期进程死即 fatal」哲学；graceful 重启留 operator/reconnect；C-1 后未来若做仅 `WIFEXITED(0)`/`SIGTERM` 重启）。
- **真机 PASS（承重假设②）**：boot 两 env（皆空格-free 过 cmdline）、**零手动推/起** → init 自启 `/bin/usnvmemu`(pid 35，args 由 env 构造)+建 256MiB backing → device shim 连上 → guest 自动出盘「OpenHCL Userspace NVMe v2.0」256MB + 4MiB IO markerMatch（oracle-1）+ oracle-2 raw backing @22577152。6 单测 + clippy/fmt 净。
- 设计/承重假设/评审 archive：`docs/superpowers/plans/2026-06-13-w6-usnvmemu-vtl2-autostart.md`。
- **未来（非本期）**：supervised restart（区分 graceful-exit vs crash，对齐 fatal-death 哲学）/ persistent backing（tmpfs backing 重启即失）/ 多设备。

---

## 4. NVMe-oF TCP target（V4 → V-followup）

> **当前状态见 auto-memory `nvme-of-tcp-current-state`**（canonical 入口）。本节是已完成 V 系列各 phase 的历史里程碑与 commit hash；逐 phase plan 文件索引在 [ROADMAP.md §6](ROADMAP.md)。整体：306 lib+integration tests + clippy 0 warning；V0..V8e + V-followup（tls/mtls/auth/dhchap/interop/prp-list/dhchap-4/tls-psk）全段 shipped；真 Linux nvme-cli plaintext discover+connect+IO 实测通过；跨进程 Python harness 22 scenarios 通过。

### 4.1 V4（admin path R2T/H2CData 闭环）

64 tests pass：
- **V4a**（`f8ef8e48`）：`r2t.rs` + `h2c_reassembler.rs` + `ttag.rs` 纯 wire layer。
- **V4b**（`ad594893`）：TcpAdminTransport + R2T → await_host_data → complete_dma 闭环；修 M-1（token 跨 cmd 单调）+ M-2（dma_read 不再 silent）。
- **V4c**（`665ba1ec`）：MAXH2CDATA_BYTES + V4_MAX_DMA_READ_BYTES；H2cReassembler.with_base_offset 修 H-1（与 Linux nvme-tcp host cumulative data_offset 兼容）+ H-2（8 MiB cap）。

### 4.2 V5（IO Read/Write + bin entry + nvme-cli interop-ready）

79 tests pass：
- **V5a**（`81dbed8e`）：IO queue 安装 + Fabric Connect qid≥1 校验 + dispatch admin/IO 二分。
- **V5b+V5c**（`0d0f6559`）：IO Read (C2HData) + IO Write (R2T/H2CData) 完整闭环；nlb=1 guard；数据持久化 roundtrip。
- **V5d**（`fc0d43b0`）：`main.rs` TcpListener:4420 + README + bin smoke test；per-conn thread + per-backing Mutex。

### 4.3 V5-followup（security + nlb cap 1→16）

V5 全部 reviewer-clean，84 lib tests。7 commits：`ffe4e92a` V5d-fix security hardening (4 CRITICAL + 5 HIGH) / `a611656f` V5d-fix-2 真 ctrlc / `2d1c1282` V5e-1 nlb 1→8 / `2ea517cd` V5e-1-fix block FORMAT/NS_MGMT / `1ba2cd5e` V5e-2 nlb 8→16 (双 PRP 8 KiB) / `0752c5e1` V5-P5 refactor / `f77c5876` V5e-2-polish。V5-P4 per-controller 推迟 V8。

### 4.4 V6（AER）

3 commits：`c6e03aa6` V6a AER cmd fast-path / `b9d3d5ec` V6b pump_one_with_events select-style + inject_aen API / `d16f6ca0` V6c e2e + polish。97 tests + clippy clean。**关键 invariant**：bin 不走 SDK `run_device`，故 controller tick() 内三处自然 fire_aen 在 V6 surface 永不触发；真生产 fire 必须通过 session `inject_aen` API。

### 4.5 V7（Discovery subsystem）

3 commits：`3f3b79b1` V7a discovery_log.rs (Figure 350/351 byte-exact) / `4681083c` V7b session discovery_mode + Connect NQN 校验 / `dcfec8ed` V7c-fix。105 tests + clippy clean。**关键修复（reviewer H-1 BLOCK）**：V7b 声称 nvme-cli discover 工作但 Identify Ctrl CNTRLTYPE 永远报 IO_CONTROLLER(0x01)→driver 按 IO fingerprint 要 Create IO SQ→discovery 白名单外→实测必失败。V7c-fix 在 CNS=0x01 分支 post-patch byte 111 = 0x02 + NN=0（spec §5.1.4 + §5.17.2.1 byte 111）。

### 4.6 V8 系列（多 conn + Disconnect + dual-listener）

5 段，124 active test + clippy clean：

| Phase | commit | 内容 |
|-------|--------|------|
| V8a | `d801306c` | Identify CNTRLTYPE builder + Vec<String> CLI portals |
| V8b | `c795f284` | Arc<SharedControllerInner> 多 conn 共享 + per-conn token slab |
| V8c | `d7b53f44` | per-conn AER routing + Drop cleanup |
| V8d | `39888a3f` | Disconnect 真清 + AER per-conn/global hard cap |
| V8f | `5a009d48` | dual-listener（`--discovery-listen` 独立 NvmeController 实例） |

**关键安全 fix（reviewer 8 轮）**：V8b C-1 per-conn token slab 防撞 pending_ios / C-2 install_admin_cq 返 Result 防协议越权；V8c H-1 dispatch_conn_id prev/restore / H-2 allocate_conn_id u32 wrap 跳 0 / M-2 Drop catch_unwind；V8d H-1 Disconnect RECFMT reject bail / H-3/H-4 AER 双层 hard cap（per-conn=8 + global=256）防 cross-tenant DoS。plan `docs/superpowers/plans/2026-06-06-phase-v8-detailed.md`。

### 4.7 V8e（tokio async refactor）

6 段，157 active test + clippy clean：`06a16176` V8e-1 tokio scaffolding + bit-exact regression gate / `785e3c5f` V8e-2 bin tokio::main + watch shutdown / `61405158` V8e-3 AsyncSession + sync/async 双轨 / `ee4f1b34` V8e-4 AER wakeup Notify channel（AER 延迟 <10ms）/ `3f5b620d` V8e-5 KATO Sleep deadline (spec §7.13) / `c6c11148` V8e-6 真并发 e2e。**关键设计决策**：Q1 controller 保 `parking_lot::Mutex` + crate 级 `#![deny(clippy::await_holding_lock)]` 编译期防滥用 + closure-only `with_controller` 不持锁跨 await；Q2 KATO `Option<Pin<Box<Sleep>>>` deadline；Q3 AER controller-wide 单 Notify + per-conn 自检过滤 spurious；Q6 shutdown 用 `tokio::sync::watch`。plan `docs/superpowers/plans/2026-06-06-phase-v8e-tokio-detailed.md`。

### 4.8 V8e-7（AsyncSession 完整 dispatch + bin 全 async）

4 段，192 active test + clippy clean：`c84c3b54` V8e-7-1 dispatch_plan sans-IO 决策表（sync/async 共用防漂移）/ `75e6e9a8` V8e-7-2 AsyncSession fabric handlers + KATO reset + Drop IO sweep / `f14051e4` V8e-7-3 admin/IO 完整 dispatch + R2T 三段式 + inject_aen_async / `eb04fcf4` V8e-7-4 bin handle_conn_async；spawn_blocking 桥退役。R2T 三段式 lock-pop / unlock-await-wire / lock-complete。plan `docs/superpowers/plans/2026-06-06-phase-v8e-7-dispatch-detailed.md`。

### 4.9 V-followup TLS / mTLS / auth / NQN-cert binding / DH-HMAC-CHAP

安全侧全段，12 commits `0534be2a`→`1646ac35`，258 tests + clippy clean。

阶段：V-followup-tls-1 stream 抽象泛化 / tls-2 rustls 0.23 + tokio-rustls / tls-3 bin TLS dual-listener + 30s handshake timeout 不 fallback / tls-4 应用层 byte-identical gate / mtls build_acceptor_with_mtls + --tls-client-ca / auth host NQN 白名单 / auth-2 NQN↔TLS cert SAN/CN binding (spec §8.13) / dhchap-1 HMAC-SHA256 building blocks / dhchap-2 ChapStage state machine / dhchap-3 bin CLI --host-secret / dhchap-3-wire AUTH_SEND/RECV PDU dispatch + admin/IO cmd gate。

CHAP 教学版 wire（简化 spec §8.13.5 4-message）：host AUTH_RECV → target C2HData(challenge 32B)+CapsuleResp → host AUTH_SEND(HMAC-SHA256 response 32B) → target CapsuleResp SC=0/0x83。response = HMAC-SHA256(secret, challenge‖hostnqn‖subnqn) 防 cross-protocol replay。CLI 总览（叠加生效）：`--tls-listen/--tls-cert/--tls-key/--tls-i-trust-this-cert`（server-auth）/ `--tls-client-ca`（mTLS）/ `--tls-bind-nqn-to-cert` / `--allow-host-nqn`（可重复）/ `--host-secret NQN=HEX`（CHAP）。生产推荐四件套。详细 commit：tls-1 `0534be2a` / tls-2 `bc2d600d` / tls-3 `ebe54a15` / tls-4 `995c8647` / mtls `793240e8` / auth `0dea1349` / auth-2 `ee820358` / dhchap-1 `b98024e8` / dhchap-2 `1e803889` / dhchap-3 `82418f35` / dhchap-3-wire `1646ac35`。plan `docs/superpowers/plans/2026-06-06-phase-v-followup-tls-detailed.md`。

### 4.10 V-followup ABC（PRP-list chunking + spec CHAP 4-msg wire + TLS PSK TP-8011）

3 段，3 commit（`eff95619`/`43040427`），300 tests + clippy clean：
- **A — V-followup-prp-list**：session-level IO chunking 16→256 LBA（8 KiB→128 KiB），透明 chunking 不动 controller PRP-list。`io_size_sweep.py` 12 场景 byte-equal + 257 LBA SC=0x18。
- **B — V-followup-dhchap-4**：spec NVMe Base 2.0c §8.13.5 4-message wire（NEGOTIATE→CHALLENGE→REPLY→SUCCESS1+FAILURE）。与 dhchap-3 simplified wire 共存（ChapWireMode 首个 AUTH_SEND 自动锁定）。reviewer 7 个 H/M/L 全修。
- **C — V-followup-tls-psk (TP-8011)**：deterministic 派生层（digest + HKDF-Expand-Label + identity 字符串），field-by-field 对齐 Linux master `drivers/nvme/common/auth.c`。HKDF-Expand-Label 严格 RFC 8446 §7.1。**未注入 rustls**——rustls 0.23 无公开 external-PSK API；待 upstream（ADR-007）。

### 4.11 真 Linux nvme-cli 互通（kernel nvme-tcp.ko 6.6.114）

WSL2 内 Linux nvme-cli 2.8 / kernel 6.6.114 真互通成功，**connect + discover 双向都通**（263 passed，commit `9fdc6991`→`024f24b6`）。

**修的 9 个 wire blockers + 全 lib regression gate**：
| # | 问题 | 修 |
|---|------|----|
| 1 | CC.EN 0→1 重置 admin CQ 到 gpa=0 | force_install_admin_cq |
| 2 | Identify Ctrl 缺 NVMe-oF mandatory fields | build_v2_bytes_with_cntrltype 完整填 |
| 3 | KAS=0 触发 mandatory | id.kas = 10 |
| 4 | Invalid MNAN value 0 | IO mnan=nn, Discovery 关 CMIC.ANA |
| 5 | Fabric IO queue Connect (qid>=1) reject | nvme_force_install_io_queue |
| 6 | MDTS=5 与 V5_NLB_MAX=16 不一致 | id.mdts = 1 |
| 7 | DiscoveryEntry struct layout 缺 eflags + rsvd0 size 错 | 加 eflags + rsvd0=20 |
| 8 | Discovery Controller SUBNQN 错 | cntrltype-conditional SUBNQN |
| 9 | **Get Log Page 忽略 LPO** | apply LPO offset to discovery log slice |

**带注解的关键教训（用户「你解决问题靠猜测吗」批评后改正）**：之前用**手算 packed struct offset** assert wire bytes（KAS=320, MNAN=524）全错，"regression test" 自己 false-positive 通过但 wire 真错 → 用户 sudo run 失败。改正：① spec layout 锚 `core::mem::offset_of!` 编译期锁定字段 offset；② spec semantics 验：真发 wire 字节 + libnvme/kernel 源比对；③ 不猜测 host 行为：看 libnvme 源；④ 每个 fix 必配 lib regression test。详 LESSONS §17 + auto-memory `review-not-optional-self-consistent-trap` / `doc-audit-via-git-log`。

**2026-06-09 host CHAP connect 硬阻塞（实测）**：WSL2 kernel `# CONFIG_NVME_AUTH is not set`——只编了 NVME_TCP/FABRICS 没 host 侧 auth。**别混**：nvme-core 的 `nvme_auth_derive_tls_psk`（target 侧）≠ host 侧 `CONFIG_NVME_AUTH`（connect auth）。dhchap-4 真互通需重编 WSL2 kernel / distro VM / 暂用 Python harness。出路见 `usnvmemu/docs/RUNBOOK_HOST_ROOT.md §1`。**plaintext connect 不受影响**。DHHC-1 key 格式 = `DHHC-1:<hmac>:base64(key‖crc32_le):`（含 CRC，用 `nvme gen-dhchap-key` 生成）。

### 4.12 V-followup-interop Python harness（14 scenarios，无 sudo/nvme-cli/fio）

`scripts/interop_py/` 用 uv-managed Python venv（stdlib only）跑 7 个 interop test、共 **14 scenarios 真生产实验通过**，覆盖 V-followup-tls/mtls/auth-2/dhchap-3-wire 全部 wire path（后续扩展到 22 scenarios 的聚合计数见 auto-memory `nvme-of-tcp-current-state`）。Test 清单（部分）：tls_smoke / tls_e2e / mtls_smoke（legit/no-cert/evil-CA）/ io_write_e2e（4 KiB + 8 KiB 双 R2T byte-equal）/ nqn_cert_binding / chap_e2e（正确/假 secret/未知 host）/ load_test（multi-thread）。

**价值**：无需 sudo/kernel module；无需 nvme-cli `--tls`（只支持 TP-8011 PSK 不能验 X.509）；uv-managed venv 零 deps；填补 lib test 之外跨进程 wire 实验空白。commit `61f0785a`（tls）/ `606ba2d6`（io_write）等。**带注解的教训**：CHAP wire fix 时发现两个 PSH 字段 layout doc 错（R2T cccid 顺序 / H2CData size 24→16），靠 server 端 C2H_TERM 反馈 + 即时改 Python parser 修。再次印证：layout 必须查源验证，不能凭记忆/手算。

### 4.13 持续开发文档中心 + 教学/生产边界（诚实）

`docs/superpowers/plans/`（注：nvme-of 早期文档；usnvmemu 项目级文档已升到 `usnvmemu/docs/`）的 ROADMAP/PRINCIPLES/LESSONS/DECISIONS 是新 phase 起手第一步。文档 audit pass（2026-06-06）：21 个历史 plan/spec 加 status banner + LESSONS +6 条 + PRINCIPLES +2 节 + DECISIONS.md 7 条 ADR。关键 commit：V-prp-list `2a4d734b` / V-dhchap-4 `eff95619` / V-tls-psk `43040427` / doc audit `6a41db11` / DECISIONS `005552c5`。

**教学/生产边界（诚实标注）**：DH-HMAC-CHAP HMAC-only（无 DH ephemeral）；TLS PSK deterministic crypto only，**未注入 rustls 握手**；`tls_psk.rs` 测试 self-consistent，缺 kernel CI 五元组 anchor；`parse_negotiate` 只看 DHCHAP authid；IO 单 cmd ≤ 128 KiB（session chunking），future production PRP-list 留 ADR-006。
