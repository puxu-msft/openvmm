# NVMe Firmware — Spec Conformance / Feature Status（权威表）

> **单一事实源**：本 firmware 对 NVMe 命令/特性的实现状态。回答"哪些做了 / 半做 /
> stub / 推迟 / out-of-scope，为什么，激活条件"就看这里。
>
> **代码是 ground truth，本文档是索引**：每行给 `file (arm @ ~line)` 锚点；具体边界/
> 理由的权威注释在代码里（本表只点位 + 一句话状态，**不复述细节**，避免漂移）。行号随
> 并发编辑会漂，以 **opcode/feature 名**为准、行号作提示。
>
> 建于 2026-06-11。维护：新增/改特性时**同步更新本表对应行**（与代码注释、commit 一起）。

## 图例

| 符号 | 含义 |
|------|------|
| ✅ | 实现且 spec-correct（含教学边界但 driver-观察正确）|
| ◐ | 部分实现（覆盖主路径，缺某子档/子语义——见"缺什么"列）|
| ⊘ | stub（接受命令但仅返固定/占位结果，无真副作用）|
| ⏸ | 故意推迟（前瞻 scaffolding 已锚定，待激活条件；§30）|
| ✗ | out-of-scope（教学 firmware 不实现，诚实拒绝优于半吊子）|

> 并发会话注：本 firmware 同期有两条并行开发线。标 **[Q]** 的行属"队列管理/CMB/
> scaffolding-SC"线（见 `plans/2026-06-11-scaffolding-sc-spec-completeness.md` +
> memory `firmware-spec-gaps-and-scaffolding-handoff`）；标 **[F]** 属"Abort/Reservation/
> MDTS/PI-separate"线（见 `plans/2026-06-11-b6b4-and-resume-plan.md`）。无标=共同基线。

---

## 1. Admin 命令（`src/controller/admin.rs` dispatch）

| Opcode | 状态 | 缺什么 / 备注 | spec | 锚点 |
|--------|------|--------------|------|------|
| IDENTIFY (0x06) | ✅ | CNS 0/1/2/3/4/5/6/8/12/13/19（NS/Ctrl/各 list；200+ 字段经 nvme_spec）；未知 CNS→zeros | §5.17 | admin.rs IDENTIFY |
| CREATE_IO_CQ (0x05) | ✅ [Q] | 含 spec 错误码（COMPLETION_QUEUE_INVALID 等）| §5.4 | admin.rs CREATE_IO_CQ |
| CREATE_IO_SQ (0x01) | ✅ [Q] | 同上 | §5.5 | admin.rs CREATE_IO_SQ |
| DELETE_IO_SQ/CQ (0x00/0x04) | ✅ [Q] | INVALID_QUEUE_IDENTIFIER/DELETION | §5.6-5.7 | admin.rs DELETE_IO_* |
| SET_FEATURES (0x09) | ✅ [F] | + persistent(Save bit，跨 reset 回灌 live 字段)（D）| §5.21.1 | admin.rs SET_FEATURES |
| GET_FEATURES (0x0a) | ✅ [F] | + SEL=2(saved) 返 saved_features（D）| §5.21.2 | admin.rs GET_FEATURES |
| KEEP_ALIVE (0x18) | ✅ | no-op success（KATO 计时在 transport 层）| §5.18 | admin.rs KEEP_ALIVE |
| ASYNC_EVENT_REQUEST (0x0c) | ✅ | AER 队列 + AEN 注入 | §5.2 | admin.rs ASYNC_EVENT_REQUEST |
| GET_LOG_PAGE (0x02) | ◐ | 上限 2 MiB（>2 MiB chaining 机制已在 device→host 路径就绪但 caller cap 未放开；真 log 都 <2 MiB）| §5.16 | admin.rs GET_LOG_PAGE |
| ABORT (0x08) | ✅ [F] | 真定位 in-flight 命令并中止 + dw0 报真结果（A1；多-DMA 完整清理）| §5.1 | admin.rs ABORT |
| FORMAT_NVM (0x80) | ✅ [F] | LBAF 0/1/2 + PI Type 0/1 + MSET 极性(B6a) + 接受 separate(B6b-1) + 清 not_ready→ready(item-1[Q]) | §5.14 | admin.rs FORMAT_NVM |
| FW_COMMIT (0x10) | ✅ | firmware slot commit；**BPID=1（写 boot partition）→ BOOT_PARTITION_WRITE_PROHIBITED**（BP write-protected，item-3[Q]）| §5.12 | admin.rs FW_COMMIT |
| FW_IMAGE_DOWNLOAD (0x11) | ✅ | chunk download（64 MiB defense cap）| §5.13 | admin.rs FW_IMAGE_DOWNLOAD |
| DEVICE_SELF_TEST (0x14) | ✅ | short/extended，tick 推进 | §5.8 | admin.rs DEVICE_SELF_TEST |
| NS_MANAGEMENT (0x0d) | ✅ | create/delete NS | §5.20 | admin.rs NS_MANAGEMENT |
| NS_ATTACHMENT (0x15) | ✅ | attach/detach | §5.19 | admin.rs NS_ATTACHMENT |
| GET_LBA_STATUS (0x86) | ⊘ | 返固定全零 LBA Status Header（NLSD=0）；backing 无 suspected-error LBA 概念，无真状态跟踪。opcode 经 M1 锚定修正（曾误填 0x1e）| §5.15 | admin.rs GET_LBA_STATUS |
| SANITIZE (0x84) | ⊘ | 状态机 + 进度 tick + 完成 AEN + SES=2 bump crypto_gen 真；但 **backing 数据不真擦写**（无 truncate/zero——核心 sanitize 动作未实现）；真 crypto-erase ✗（无加密层，D④）| §5.24 | admin.rs SANITIZE |
| DOORBELL_BUFFER_CONFIG (0x7c) | ✅ | DBBUF 真实现（shadow doorbell DMA-poll + event_idx）| §5.8/§7.13 | admin.rs DOORBELL_BUFFER_CONFIG |
| LOCKDOWN (0x24) | ✅ | per-opcode 禁用/启用 | §5.18(2.0) | admin.rs LOCKDOWN |
| DIRECTIVE_SEND/RECEIVE (0x19/0x1a) | ⊘ | Send 恒 no-op success（不真分流 Stream）；Receive 恒返全零（无 directive 状态）| §5.9-5.10 | admin.rs DIRECTIVE_* |
| SECURITY_RECEIVE (0x82) | ◐ | 仅 SECP=0 Protocol List | §5.28 | admin.rs SECURITY_RECEIVE |
| SECURITY_SEND (0x81) | ✗ | 无真 Security Protocol（TCG OPAL 等）→ INVALID_FIELD | §5.27 | admin.rs SECURITY_SEND |
| VIRTUALIZATION_MGMT (0x1c) | ✗ | 不实现 SR-IOV/VF → INVALID_FIELD | §5.22 | admin.rs VIRTUALIZATION_MGMT |
| （未列 opcode） | — | 一律 INVALID_OPCODE（spec 正常路径，driver 视作"特性不支持"）| — | admin.rs `_ =>` |

## 2. NVM (IO) 命令（`src/controller/io.rs` dispatch_io）

| Opcode | 状态 | 缺什么 / 备注 | spec | 锚点 |
|--------|------|--------------|------|------|
| READ (0x02) | ✅ [F] | plain + PI(PRACT=1 inline) + **separate/inline PRACT=0 全 nlb**(B6b-3/4 + B6c/3) + **#4c PRP1 页内偏移全 tier** | §3.x | io.rs READ |
| WRITE (0x01) | ✅ [F] | 同 READ + **separate/inline PRACT=0 全 nlb**(verify-before-store) + **#4c PRP1 页内偏移全 tier** | §3.x | io.rs WRITE |
| COMPARE (0x05) | ✅ | plain NS only（PI/meta NS→INVALID_FIELD）+ fused C&W 原子 | §3.x | io.rs COMPARE |
| FLUSH (0x00) | ✅ | flush 全 NS volatile | §3.x | io.rs FLUSH |
| WRITE_ZEROES (0x08) | ✅ | 范围写零（无 host 数据传输）| §3.x | io.rs WRITE_ZEROES |
| DSM (0x09) | ✅ | dataset mgmt + range 冲突检测 | §3.x | io.rs DSM |
| COPY (0x19) | ◐ | Simple Copy + range 冲突检测（plain NS）；MCL/MSRC/MSSRL 仅 Identify 广告、COPY 臂**未强制** | §3.3.7 | io.rs COPY |
| VERIFY (0x0c) | ✅ | 设备内校验（不向 host 传数据）| §3.x | io.rs VERIFY |
| ZONE_MGMT_SEND (0x79) | ✅ | ZNS open/close/finish/reset/offline + ZSA 矩阵 | ZNS§5 | io.rs ZONE_MGMT_SEND |
| ZONE_MGMT_RECEIVE (0x7a) | ✅ | zone report（上限 2 MiB）| ZNS§5 | io.rs ZONE_MGMT_RECEIVE |
| ZONE_APPEND (0x7d) | ✅ | 单-PRP（≤4 KiB，远低于 MDTS）| ZNS§4.3 | io.rs ZONE_APPEND |
| RESERVATION_REGISTER (0x0d) | ✅ | register/unregister/replace + PTPL | §6.13 | io.rs RESERVATION_REGISTER |
| RESERVATION_ACQUIRE (0x11) | ✅ [F] | Acquire + **Preempt 完整 SPC-3/§6.11**(B1)；Preempt-and-Abort 的 abort 分量=单 host 边界 | §6.11 | io.rs RESERVATION_ACQUIRE |
| RESERVATION_RELEASE (0x15) | ✅ | release/clear | §6.15 | io.rs RESERVATION_RELEASE |
| RESERVATION_REPORT (0x0e) | ◐ | 24-byte registrant；**EDS=1(64-byte HOSTID) ✗**；builder 漏 ptpls@19 | §6.14 | io.rs RESERVATION_REPORT |
| WRITE_UNCORRECTABLE (0x04) | ✗ | backing 无 ECC 概念 → INVALID_OPCODE | §3.x | io.rs WRITE_UNCORRECTABLE |
| （未列 opcode） | — | INVALID_OPCODE | — | io.rs `_ =>` |

## 3. 跨命令特性（数据路径 / 协议机制）

| 特性 | 状态 | 缺什么 / 激活条件 | 锚点 |
|------|------|------------------|------|
| **PI 内联(extended LBA, MSET=1) PRACT=1** | ✅ | controller gen/strip，data-only wire，interleave 存盘 | io.rs PI 路径 + completion.rs NvmWritePi* |
| **PI separate(MSET=0) PRACT=0** | ✅ [F] | host 经 MPTR 供 PI tuple + verify-before-store；**全 nlb**（单 LBA B6b-2/3、多 LBA N>2 B6b-4 PRP-list）；**#4c PRP1 任意页内偏移 O∈[0,4095]**（nlb=1 Single/Dual、nlb=2 Dual/List、N>2 List；per-host-segment 拼回→按 data_bytes 重切，全 tier 零 spec-legal 例外，#4c-b P3②/①）| completion.rs SepMeta*Accum + sep_meta_write_finalize + io.rs |
| **PI inline(MSET=1) PRACT=0**（host extended-block） | ✅ [F] | host 经 PRP 供完整 4104 extended block + verify-then-store；**全 nlb**（nlb=1 dual-PRP B6c、nlb≥2 PRP-list B6c-3）；**#4c PRP1 任意页内偏移**（nlb=1 O≤4088 Dual split 左移 (4096−O,8+O) / O>4088 升 List、nlb≥2 List，全 tier 零例外，#4c-b P3③/#4c-a）| io.rs inline 路径 + completion.rs inline_meta_write_finalize / PrpListOp inline |
| PRCHK 逐项门控（cdw12 28:26） | ⏸ [F] | 未解析→一律 verify(over-strict，安全方向)；真做时按 bit | completion.rs verify 注释 |
| **PRP** 单/双/单-list | ✅ | ≤513 页(~2 MiB) | io.rs + mod.rs PrpListOp |
| **PRP-list chaining(>2 MiB) device→host** | ✅ [F] | 机制就绪(C1②)；**生产路径激活待 MDTS 抬高 / report cap 放开**(forward scaffolding §30) | completion.rs NvmReadPrpListFetch |
| PRP-list chaining host→device(write) | ⏸ [F] | 不可达(MDTS=128KiB 锁死)，按 §30 不投机实现 | — |
| **SGL** (PSDT 01/10) | ✅ [Q] | segment chain + Bit Bucket（R2d）；**CMB-relative sub_type=1（Offset）**：CMB 启用时**放行 + rebase**（`cba+offset`，CMB-P2/P5）、越界 offset≥size→`SGL_OFFSET_INVALID`(0x16)（P5 严格）、CMB off 时→`SGL_INVALID_USE_OF_CMB`(0x12)；**3 路径经共享 `subtype_to_sc`+`resolve_sgl_address` 一致**；**PSDT/SGL 模型对齐 spec（E0）**：`PSDT∈{01,10}` 都=SGL data，data 单/碎由 **DPTR descriptor 类型**（DataBlock→单 PRP 复用 / (Last)Segment→scatter）定、**与 PSDT 无关**；PSDT 只区分 *metadata* 形态（01=平坦 MPTR / inline 吸收；10=metadata-SGL）；`PSDT=11`→`INVALID_FIELD`；**SGL×PI separate-meta ✅（PRACT 0/1，data 经 SGL frag scatter-gather + PI tuple 经 MPTR 双门控、READ Bit Bucket 跨 LBA 正确丢弃，P-B）；inline-meta ✅（PRACT 0/1，extended-block 流 nlb×4104 tuple 在流内/无 MPTR、extended block 跨 fragment 正确重组、PRACT=1 strip data-only，P-C）；metadata-SGL（PSDT=10）⏳ E1（SGLS bit19=`NVME_CTRL_SGLS_MSDS` 未 advertise → 现 reject INVALID_FIELD，spec 合法）** | src/sgl.rs + io.rs SGL 路径 |
| **CMB**（Controller Memory Buffer，spec § 8.x）| ✅ | **双模 + 配置可选**（`--cmb-mode off\|trap\|map`）：CMBLOC/CMBSZ/CMBMSC/CMBSTS 寄存器 + CMBMSC 时序状态机（CRE 先于 CMSE）；**trap**=REGION_RW 转发 backing（任意 transport）/ **map**=memfd backing + SCM_RIGHTS fd-pass + client mmap 零拷贝（仅 vfio-user，OpenHCL 无 mapper 降级 trap，TCP 不适用）；CMB 内 SQ/CQ/PRP/SGL-data；SGLS bit20 SAOS 条件 advertise。两模均有 e2e | regs.rs+controller/{mmio,cmb}.rs + vfio_user_transport/{session,memfd_region}.rs + vfio_user_pci_device + main.rs |
| **MDTS** 强制 | ✅ [F] | 全数据命令；计入 inline metadata(C1①)；值=5(128 KiB) | src/regs.rs MDTS_MAX_BYTES + io.rs |
| **Fused** Compare&Write | ✅ | 原子；**>1page(超原子能力)→ATOMIC_WRITE_UNIT_EXCEEDED(0x14)**（item-2[Q]，共享 `fused_cw_reject_sc`，本地+fabric 一致）| io.rs/completion.rs fused + mod.rs fused_cw_reject_sc |
| **NS-not-ready 门** (0x82) | ✅ [Q] | not_ready NS 的 IO + fused C&W→NAMESPACE_NOT_READY；`--not-ready-nsid` 触发、Format 转 ready、Identify CNS 0x08 NSTAT.NRDY 跟随（item-1）| io.rs/mod.rs not_ready 门 + admin.rs NSTAT |
| **Boot Partition** (spec § 8.13) | ✅ [Q] | `--boot-partition-file` 装只读出厂镜像→BPINFO.BPSZ>0 + Boot Partition Read（BPRSEL 触发→DMA content 到 BPMBL，BRS 跨回调状态机）；write-protected（FW Commit BPID→0x11e）；无文件=BPSZ=0 不广告（item-3）| mmio.rs BPINFO/BPRSEL + completion.rs pending_boot_reads + admin.rs FW_COMMIT |
| **DBBUF** shadow doorbell | ✅ | DMA-poll + event_idx + 自续深度 cap | mod.rs shadow poll |
| **persistent features**(Save) | ✅ [F] | 跨 reset 回灌(含 mirror-backed live 字段)（D）| enable.rs + admin.rs |
| **CSTS.CFS** on shutdown-flush 失败 | ✅ [F] | spec § 3.1.4.5（D）| enable.rs process_shutdown |
| **中断投递 / MSI-X** | ✅ | per-CQ interrupt_vector + interrupt_enabled + fire_interrupt | mod.rs post_cqe / CREATE_IO_CQ |
| **Interrupt Coalescing**（FID 0x08） | ◐ | threshold/time 真生效（tick 时间-flush）；AGGR_TIME 实际下限=tick ~100 ms（教学边界）| admin.rs SET_FEATURES 0x08 + mod.rs tick |
| **Log pages**（GET_LOG_PAGE LID） | ✅ | 真 build：Error(0x01)/SMART(0x02)/FW(0x03)/Changed-NS(0x04)/Self-test(0x06)/ANA(0x0c)/Reservation-Notif(0x80)/Sanitize(0x81)/Discovery(0x70) 等；未知 LID→zeros | admin.rs GET_LOG_PAGE + logs.rs build_* |
| **AEN 事件源** | ✅ | Self-test done / Sanitize done / Error / Reservation Notification / ANA Change 触发 fire_aen | mod.rs fire_aen 调用点 |
| mset=1 separate metadata（B6 标签历史） | — | 注：早先 inventory 的"mset=1 separate"基于 firmware **写错**的极性注释；B6a 已修正(MSET=1=内联/MSET=0=separate)。真 separate 见上"PI separate"行 | — |

## 4. 推迟 / out-of-scope（含激活条件）

| 项 | 类 | 激活条件 / 理由 | 归属 |
|----|----|---------------|------|
| B6b-4 多 LBA separate metadata | ✅ | 已落地（`8d87e7812` + #4c-a/#4c-b PRP1 偏移）；见上 "PI separate" 行 | [F] |
| PRP-list chaining 真激活 | ⏸ | 抬 MDTS 让 IO Read >2 MiB（注意 nvme-of transport nlb cap 独立）| [F] |
| inline NS PRACT=0 / PRCHK 门控 | ✅ | 已落地（B6c/B6c-3 + #4c-b P3③ PRP1 偏移）；见上 "PI inline" 行；PRCHK 门控已接（cdw12 PRINFO 逐项）| [F] |
| CMB-SGL SC 一致（条件化）| ✅ | `sgl.rs` 共享 `subtype_to_sc`+`resolve_sgl_address`，3 路径统一：CMB off→0x12 / 启用越界→0x16 / 启用合法→放行 rebase（CMB-P2/P5）| [Q] |
| NS-not-ready (0x82) | ✅ | item-1：per-NS `not_ready` + `--not-ready-nsid` 触发 + IO/fused 门 + Format-readiness + NSTAT.NRDY 跟随（`488ffaa8`）| [Q] |
| AWUN (0x14) | ✅ | item-2：fused C&W >1page(超原子能力)→ATOMIC_WRITE_UNIT_EXCEEDED；普通 Write>AWUN 不 reject（spec：仅不保证原子）（`5b8dafa1`）| [Q] |
| boot-partition (0x11e) | ✅ | item-3：`--boot-partition-file` 装只读出厂镜像→广告 BPSZ>0 + 服务 Boot Partition Read（BPRSEL→DMA→BMBL，BRS 状态机）+ FW Commit BPID→BOOT_PARTITION_WRITE_PROHIBITED（write-protected）| [Q] |
| Reservation Report EDS=1(64-byte HOSTID) + ptpls@19 | ◐→⏸ | 多 host HOSTID 扩展 + builder 补字段 | [F] |
| Security(TCG OPAL) / Virtualization(SR-IOV) | ✗ | 偏离教学核心，真实现工程量大；诚实 INVALID_FIELD 优于半吊子 | — |
| Write Uncorrectable | ✗ | backing 无 ECC 概念 | — |
| 真 crypto-erase sanitize | ✗ | 无加密层；crypto_gen 模拟代际 | — |
| 4 个无牙测试上牙 / async 错误码普查 | — | 测试质量项，见 `TEST_COVERAGE_PROGRESS.md` follow-up（**特性完整性见本表，测试覆盖率见那里**）| — |

---

## 5. 指针（其余文档的分工）

- **测试覆盖率/质量** → `docs/TEST_COVERAGE_PROGRESS.md` + `docs/TEST_QUALITY.md`（关注"测试有没有牙"，非特性完整性）。
- **会话续作执行细节** → `docs/plans/2026-06-11-*.md`（**临时**，完成即过期；"整体下一步"看本表）。
- **教训** → `../../docs/LESSONS.md`（§20 self-consistent / §23·26 无牙 / §30 scaffolding）。
- **架构决策** → `../../docs/DECISIONS.md`（ADR）。
- **本表只列状态 + 锚点**；每个边界/stub 的**权威理由在代码注释**（顺锚点去读）。
