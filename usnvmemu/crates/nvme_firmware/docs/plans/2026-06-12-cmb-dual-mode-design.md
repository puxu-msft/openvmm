# NVMe CMB 双模设计 —— trap-based + map-based 并存,配置可选

**日期**：2026-06-12
**状态**：✅ 已实现 + **L4 真机完全达成**（P1a–P5 全部落地，每相独立 review；两模均有 e2e。2026-06-13 真 QEMU + 真 Linux 6.8 nvme 驱动 trap+map 双模 SQ-in-CMB 全 PASS，达成前修了两个真机暴露的 firmware 寄存器 bug — CMBSZ 位布局 + CMBMSC 跨 reset 生命周期，commit 3a1779c1d）
**归属**：`nvme_firmware`（主）+ `vfio_user_transport` / `vfio_user_wire` / `pcie_device_core`（协议）+ `vfio_user_pci_device`（OpenHCL client，仅 map 模式）
**前置可行性**：见 `usnvmemu/experiments/2026-06-12-vtl-memory-direction-feasibility/`（VSM 单向墙 + transport 不对称的三重证据）。

## 0. 目标与教学完整性

给用户态 NVMe firmware 加 **Controller Memory Buffer（CMB）**——NVMe 规范里"设备把一段自己的内存经
BAR 暴露给 driver 直接读写"的特性。本项目是**教学项目,完整性优先**,故**两种实现模式并存、运行时配置
可选**:

- **trap-based CMB**：guest 访问 CMB BAR → trap → transport 经 `REGION_READ/WRITE` 把读写转发到
  firmware 的 CMB backing。**功能正确,非零拷贝**(每次访问 = VM-exit + message 往返)。
- **map-based CMB**：firmware 的 CMB backing 经 fd 暴露,client `mmap` 后 guest **零拷贝直访**。

**关键设计洞察**:**两模共享同一份 CMB backing**——区别只在 transport 怎么把它暴露给 guest。
firmware 侧的 CMB **控制逻辑**(寄存器、CMB-vs-DMA dispatch、SGL CMB-relative)两模一致;模式选择是
transport/runtime 的事,不污染 firmware core。**注意(architect 复核修正)**:两模一致仅限*控制逻辑*——
**内存序语义不同**:trap 模式天然串行(每访问 wire 往返),map 模式是 guest↔firmware **真并发**共享同一
backing。CMB 内 SQ/CQ 的 producer/consumer 同步在 map 模式需显式模型(doorbell 仍在 BAR0、仍 trap,作
同步点 + acquire/release)。见 §10。

## 1. 可行性结论(决定每模在哪条 transport 上成立)

详证见 experiment。净结论:

| 模式 | guest 访问机制 | OpenHCL(vsock/vfio-user-in-underhill) | QEMU(vfio-user over AF_UNIX) | NVMe-oF TCP |
|---|---|---|---|---|
| **trap** | trap → message → firmware backing | ✅ | ✅ | ✅(无 PCIe BAR 概念,CMB 退化为协议内缓冲,见 §6) |
| **map** | client mmap server region fd,guest 直访 | ⚠️ **走不通**(underhill `shared_mem_mapper:None`,无 `MemoryMapper`;唯一推测路径见 §5,需真机 POC) | ✅(QEMU 拥有 guest 内存映射,能 mmap region fd) | N/A |

**根因**(单向墙):VTL0 的 GPA→SPA 表归 host 拥有;underhill(VTL2)无 `MemoryMapper`、无 add-backing
原语,故**无法 direct-map 任何内存进 VTL0 BAR**。QEMU/OpenVMM-host 拥有 guest 内存表,故能。
→ **map 模式的零拷贝能力取决于 client/host 能否 map**,firmware core 保持 agnostic。

## 2. 架构:同一 backing,两种暴露

```
              ┌──────────────────────── firmware core (runtime-agnostic) ─────────────┐
              │  NvmeController.cmb: Option<CmbState>                                  │
              │    backing: Box<dyn SharedRamRegion>   ← as_bytes / as_bytes_mut       │
              │    cba / size / enabled(CMSE)                                          │
              │  io.rs: access_guest(gpa) → gpa∈[cba,cba+size) ? backing : ctx.dma_*   │
              └───────────────────────────────────────────────────────────────────────┘
                          ▲ backing 由 transport 注入,impl 因 transport 而异
        ┌─────────────────┴───────────────┐                  ┌────────────────────────┐
        │ vfio_user_transport             │                  │ test / 中立              │
        │  SharedRamRegion = memfd-backed │                  │  SharedRamRegion = Vec   │
        │  ┌── trap 模式 ──────────────┐  │                  └────────────────────────┘
        │  │ GET_REGION_INFO: READ|WRITE│  │
        │  │ 访问走 REGION_READ/WRITE   │  │
        │  └────────────────────────────┘  │
        │  ┌── map 模式 ───────────────┐  │
        │  │ GET_REGION_INFO: +FLAG_MMAP│  │
        │  │  reply 附 memfd (SCM_RIGHTS)│  │
        │  └────────────────────────────┘  │
        └──────────────────────────────────┘
```

- **`SharedRamRegion` trait**(新增,放 `pcie_device_core`,`#![forbid(unsafe_code)]` 下只定义接口):
  `as_bytes(&self)->&[u8]` / `as_bytes_mut(&mut self)->&mut[u8]` / `gpa_base(&self)->u64` / `len`。
- vfio-user 的 impl 用 **memfd**(`memfd_create`,server 自持→更安全,信任方向反转):既可被 firmware
  本地读写(trap 模式服务 + map 模式下 firmware 反手访问),又可经 fd 暴露给 client(map 模式)。
- 测试/中立 impl 用 `Vec<u8>` / 匿名 mmap。

## 3. 配置面(运行时选择)

firmware CLI(clap,`src/main.rs`,仿现有 `--zns-nsid`):
- `--cmb-mode <off|trap|map>`(default `off` —— 不 advertise CMB,保持现状)。
- `--cmb-size <bytes>`(default 如 2 MiB;决定 CMBSZ)。
- `--cmb-bir <n>`(default 独立 BAR,如 BAR2;避开 BAR0 的 doorbell/MSI-X 副作用区与 sparse-mmap 复杂度)。

模式协商:`map` 模式在 transport 不支持 mmap region 时(如 OpenHCL underhill)**自动降级 trap + 日志告警**
(教学可见),不静默失败(对齐项目 silent-failure 纪律)。

## 4. 分层集成图(file:line 锚定,三层勘察汇总)

### firmware 层（`nvme_firmware`）
- `src/regs.rs`:`Reg` enum 补 `Cmbmsc=0x50`/`Cmbsts=0x58`;新增 CMBSZ(SZU/SZ/SQS/CQS/LISTS/RDS/WDS)、
  CMBLOC(BIR/OFST)、CMBMSC(CRE/CMSE/CBA)位常量;`build_cap()`(regs.rs:130) 置 CAP.CMBS(bit57)。
- `src/controller/mmio.rs:38-59`:`0x38/0x3c` 返真 CMBLOC/CMBSZ;新增 `0x50` CMBMSC 读写(size-aware
  仿 ASQ/ACQ `mmio.rs:106`,解析 CBA+CMSE+CRE 触发启用/基址编程);`0x58` CMBSTS。
- `src/controller/mod.rs`:`NvmeController` 加 `cmb: Option<CmbState>`;`open()`(mod.rs:2275)/`disable()`
  初始化/清理;`describe()`(mod.rs:3907) 据 cmb-bir push CMB `BarLayout`。
- `src/controller/io.rs`:所有 `ctx.dma_read/write` 前插 `access_guest(gpa,len)`——gpa∈CMB → 直接读写
  backing 切片(同步,需合成立即完成 token 兼容 `on_dma_complete` 状态机),否则走 DMA。
- `src/sgl.rs:96-131`:`subtype_to_sc()` 在 CMB 启用时放行 `sub_type=1`(CMB-relative),三路径
  (`parse_sgl_list`/`resolve_data_pointers`/`validate_segment_pointer`)自动一致。
- `src/cmd.rs`:Identify Controller 相关位(SGLS 等)按是否支持 CMB-relative SGL 调整。

### 协议/transport 层（`pcie_device_core` / `vfio_user_wire` / `vfio_user_transport`）
- `pcie_device_core/src/describe.rs:30`:`BarLayout` 加"可 mmap + backing"语义字段;新增 `SharedRamRegion` trait。
- `vfio_user_wire/src/proto.rs`:`region_flags::MMAP=0x4`、`RegionInfoPayload.cap_offset/offset` **字段已在**,
  填值即可(整-region mmap,无需 sparse cap)。
- `vfio_user_transport/src/session.rs:315-391`:`handle_get_region_info` 扩展服务 CMB BAR(当前只 BAR0/CONFIG);
  trap 模式回 READ|WRITE;map 模式 +FLAG_MMAP 且走**带 fd 的 reply**(现 `reply()` helper 不带 fds,需扩展
  或直调 `write_message(...,&[cmb_fd])`,SCM_RIGHTS 底层 `framing.rs:255` 已支持)。
- `vfio_user_transport/src/dma.rs`:CMB backing 的 memfd 构造可借鉴 `map_dma_fd`(dma.rs:456) + 测试
  `memfd_with` 模式;CMB 访问(trap 模式)服务可借鉴 `mmap_read/write`(dma.rs:253) 的零拷贝命中。

### OpenHCL client 层（`vfio_user_pci_device`,仅 map 模式相关）
- trap 模式:**QEMU-as-client 端零改**(QEMU 自己处理 BAR);**OpenHCL `vfio_user_pci_device` 端非零改**
  ——当前 `resolver.rs:186-194` 只有 `bar0`+`bar4(msix)`,**无第二数据 BAR**,独立 CMB BAR 需新增
  `.barN(cmb_size, Intercept)` + worker 转发路径。两端 server `session.rs:356-379` 也都要扩 region_info
  (现 BAR1-5 硬编码 size=0)。这是真改动,见 §8 P3b。
- map 模式:**当前走不通**(无 mapper)。§5 的推测路径若 POC 通过再接线。

## 5. map-on-OpenHCL 的唯一推测路径(独立 track,需真机 POC,不阻塞主线)

唯一与单向墙不冲突的路:**CMB backing 用 guest RAM(非 VTL2 私有)**,把 CMB BAR 的 GPA **别名**到那段
guest RAM——guest 直访(本是它的 RAM)、firmware 经 `/dev/mshv_vtl_low` 反手够到(正向、已证 W6c)。
**可能**是 GET `create_ram_gpa_range` 的"别名既有 guest RAM"语义能做的。但 OpenVMM host handler 是
FAILED stub、flags 仅 `rom_mb`、可写 BAR 窗口别名未知 → **未验证承重假设,真 Hyper-V POC 才能定**。
按 `poc-before-settling-design`:**先 POC,后设计/接线**。

## 6. NVMe-oF TCP 的 CMB(完整性补充)

TCP transport 无 PCIe BAR 概念。CMB 在 NVMe-oF 语境对应 **in-capsule data** / **host/controller memory
模型的退化**——教学完整性下记录:CMB 的 BAR-exposed 形态是 PCIe-specific,TCP transport 上 CMB 不适用
(CMBLOC/CMBSZ 返 0,与现状一致);相关数据共享走 in-capsule data(已在 NVMe-oF track)。**不在本设计的
实现范围**,仅标注边界。

## 7. reuse vs 净新增

**reuse**:`BarLayout`/`config_space()` BAR 编码;`sgl::subtype_to_sc` 集中 classifier;`Namespace` 的
mmap fast-path/fallback(`mod.rs:1093`);`framing.rs:write_message` 的 SCM_RIGHTS 发送(map 模式发 fd);
`region_flags::MMAP`+`cap_offset`/`offset` 字段;`map_dma_fd`/`memfd_with` 的 memfd+mmap 模式。

**净新增**:`SharedRamRegion` trait + 各 transport impl;CMB 寄存器(CMBMSC/CMBSTS + 位常量 + CAP.CMBS);
`access_guest` CMB-vs-DMA dispatch;`handle_get_region_info` 扩展(CMB BAR + 带 fd reply);CLI `--cmb-*`;
client 第二 BAR 暴露(若独立 BAR);(map 模式)client 收 region fd + mmap 路径。

## 8. 开发节奏(分阶段,每阶段 = 语义单元,结尾 review + commit)

| Phase | 内容 | crate | 承重 | POC gate | 可独立测 |
|---|---|---|---|---|---|
| **P1** | firmware CMB 寄存器 + `CmbState` + `SharedRamRegion`(Vec impl)+ `access_guest` dispatch + CMBMSC 启用 | nvme_firmware + pcie_device_core | 低 | —— | ✅ 单元测试(Vec backing,无 transport) |
| **P2** | SGL CMB-relative 放行 + Identify 位 + CMB 内 SQ/CQ/data 端到端(本地) | nvme_firmware | 低 | —— | ✅ |
| **P3** | trap 模式 transport:CMB BAR region_info(READ\|WRITE)+ REGION_READ/WRITE 服务 backing + memfd impl | vfio_user_transport | 低 | —— | ✅ QEMU e2e(trap) |
| **P4** | map 模式 transport:FLAG_MMAP + 带 fd reply;client 收 fd mmap | vfio_user_transport + vfio_user_device | 中 | **region-mmap fd-pass**(Linux 原生,POC-1 镜像) | ✅ QEMU e2e(map,零拷贝) |
| **P5** | CLI `--cmb-mode/size/bir` + 模式协商/降级日志；**+ P2 复核遗留**：① 条件化置 Identify SGLS "offset support" 位（CMB 启用时才 advertise，否则 driver 不发 CMB-relative SGL，功能就绪但无人触发）；② CMB-relative offset ≥ size 改严格返 `SGL_OFFSET_INVALID`(0x16)（现 lenient 走 DMA，见 sgl.rs `resolve_sgl_address` 与测试 `cmb_relative_offset_out_of_window_falls_to_dma_lenient`） | nvme_firmware | 低 | —— | ✅ |
| **P6**(独立) | map-on-OpenHCL §5 真机 POC | experiment | 高 | **create_ram_gpa_range 别名可写窗口**(真 Hyper-V) | 真机 |

**节奏纪律**:P1→P5 顺序推进(P4 依赖 P3);每 Phase 结尾过对应 subagent review(rust-reviewer)再 commit
(语义单元粒度,pathspec 限定);P6 独立、不阻塞 P1-P5;改动同步 SPEC_CONFORMANCE / README / codemap。

## 9. 风险

1. **token 模型 vs 同步本地访问**:CMB 内 SQ/CQ/data 是同步内存,需合成立即完成 token 兼容 io.rs 异步状态机。
2. **CBA 运行时编程**:CMSE 未置位前 CMB 不可用;维护 guest-CBA ↔ backing 偏移映射。
3. **map 模式副作用**:CMB 区**绝不能**含 doorbell/CC(那些需副作用);独立 BAR 天然隔离。
4. **SIGBUS/生命周期**:CMB memfd 由 server owner 持有,生命周期 ≥ client 映射(session 级)。
5. **隔离 VM**:CMB 直访绕 bitmap 门控,CVM/software-isolated 下 map 模式结构性不可用(scope 到非隔离)。
6. **跨路径一致性**:SGL CMB-relative 放行须三路径全覆盖(classifier 集中化已降风险)。

## 10. Architect 复核修订（实现前必纳入）

architect 对抗复核结论:主架构("同一 backing 两模暴露")**成立**,token 模型可落地(见下),§5 标注纪律正确。以下修订**实现前必纳入**:

### 必改（correctness）
1. **`SharedRamRegion` 去掉 `gpa_base()`**(抽象泄漏):CBA 是 guest 经 CMBMSC 编程的 **firmware 状态**,不是 backing 的固有属性。trait 只留 `as_bytes`/`as_bytes_mut`/`len`;CBA↔offset 映射留在 `CmbState`。否则 map 模式下 client 映射的 region 与 firmware 记的 gpa_base 成两个真相源。
2. **token 合成的归属队列与重入边界**(P1 真正会翻车点):CMB 命中走的"立即完成 token"**不是新机制**——它是 vfio transport 同步合成的退化版(`session.rs:714-737` dma_read 同步往返→`push_back` DmaCompletion→`drain_dma_completions` 栈外投递,契约见 `device.rs:113-117`)。**但** firmware core 拿不到 transport 队列,故需在 `NvmeController` 内置**本地 completion 待投递队列**,由 `tick`/dispatch 收尾 drain 喂 `on_dma_complete`。**绝不能在 `access_guest` 调用栈内同步递归调 `on_dma_complete`**(会破坏 `mod.rs:2735-2763` 的自喂 re-read 无限循环防护)。**P1 验收必须加:CMB 合成 completion 入本地队列、栈外 drain、非重入。** 别引入 future(更脏、污染纯同步 PendingOp 模型)。
3. **"trap 几乎零改"措辞已修**(见 §4.3):仅对 QEMU-as-client 成立;OpenHCL client 要新增第二 BAR + worker 转发,两端 server 扩 region_info。

### 必补（设计空白，§9 风险表已偏薄）
4. **reset 语义**(HIGH,原文全缺):CC.EN 1→0(controller reset)与 PCIe FLR(`device.rs:96`)下 CMBMSC.CRE/CMSE 是否清零、backing 内容是否清——必须显式定义。
5. **CRE vs CMSE 时序**(MEDIUM):spec 要求 CRE 先于 CMSE;状态机须校验非法组合(CMSE=1 而 CRE=0 拒绝),不做 magic。
6. **map 模式并发内存序**(HIGH):guest 经 mmap + firmware 经 memfd 本地映射**同时**读写同一 backing;CMB 内 SQ/CQ 是 producer/consumer 共享。需明确同步模型:doorbell(BAR0,仍 trap)作同步点 + acquire/release/fence。**这正是"两模一致"不成立于内存序的地方(trap 串行 vs map 并发)。**
7. **reconnect 跨 session backing 持久**(HIGH):项目有 W6c reconnect/revive 能力。session 级 backing 生命周期在 reconnect 下**不够**——reconnect 换 fd 会留 guest 悬空映射 + CMB 内容丢失。CMB backing 应**跨 reconnect 持久**,reconnect 后 client 重 GET_REGION_INFO + 重 mmap。
8. **CMBSZ 单位/对齐**(LOW):`--cmb-size` 须校验为 SZU 合法倍数;CBA 按 CMBSZ 粒度对齐;CMBLOC.BIR 与 client 实际 BAR 槽位对齐(64-bit BAR 占两槽)。

### §5 命题收紧
§5 真正要先验的 POC 命题**不是**"别名既有 guest RAM",而是 **"`create_ram_gpa_range` 造的 RAM range 能否呈现为一个 vfio-user 设备的 BAR(driver 经 CMBLOC.BIR/OFST 在 BAR 内寻址命中它)"**——`i440bx` 的先例是 host-bridge 的 ROM/RAM 区,**非设备 BAR**,这层对应无先例,是推测里最薄一环。另:CMB backing 用 host RAM 而非设备 SoC RAM,是相对真 NVMe CMB 的**语义偏移**,教学文档应写明。

### §8 分阶段修订
- **P3 拆为 P3a / P3b**:
  - **P3a**(低承重,QEMU e2e):server `session.rs` region_info + REGION_READ/WRITE 服务 CMB backing。
  - **P3b**(中承重,跨 crate):OpenHCL `vfio_user_pci_device` 暴露第二 BAR + worker 转发(当前无第二 BAR 先例)。
- **fd-pass spike 前移**:P4 的"带 fd 的 GET_REGION_INFO reply"(`session.rs` 现 `reply()` 不带 fd,需扩展)是 map 模式唯一真承重点,应作 **P4 准入 spike**(可与 P1 并行的 Linux 原生最小验证),不放 P4 内部靠后。
- P1 验收加第 2 条的"本地队列 + 栈外 drain + 非重入"。

## 11. 验证缺口与后续条件（记录不丢弃）

CMB 验证金字塔自下而上 4 层；前两层**已做**,后两层是**已知缺口**(非遗漏,本就超单测/in-process 范畴),在此显式记录 + 标注"何时值得补":

| 层 | 状态 | 覆盖什么 | 何时补才有意义 |
|---|---|---|---|
| **L1 单元/行为**(234 测试) | ✅ 已做 | 寄存器/dispatch/非重入/SGL/strict-0x16/SGLS/negotiate/BAR 访问/`apply_cmb_*` glue | —— |
| **L2 in-process e2e**(trap + map) | ✅ 已做 | 真 controller+session/socketpair:trap REGION_RW 往返 + map memfd 零拷贝(断言无 REGION_RW) | —— |
| **L3 binary-startup smoke**(起真 server + 连 client 跑通 main run 路径) | ❌ 缺口 | `run_vfio_user`/`run_main` 的**启动胶水**:negotiate→apply→serve 的时序、feature-gate、真 socket 起服务。构成它的零件全测了,但"二进制真起来并服务 CMB"这层没自动化 | **main.rs 启动路径重构后** / **CI 要 gate"二进制能起 CMB"** / 怀疑启动时序回归时。当前边际价值低(零件已测 + glue 已单测),是回归守护性质 |
| **L4 真机 guest-boot**(真 NVMe 驱动驱动 CMB) | ✅ **完全达成**(2026-06-13,QEMU+真 Linux 6.8,trap+map 双模) | 真 `nvme.ko` 把 IO SQ 放进 CMB(0xfe000000)+ firmware 从 CMB backing 取 SQE(CMB-RESIDENT-ACCESS)+ 真 write/read/flush 全 PASS,三 oracle(guest IO/host backing/CMB-USED)一致。**达成前修了两个 firmware 真机 bug**(均 self-consistent trap):① CMBSZ 位布局非 spec-aligned(SQS 编 bit4 而非 §3.1.14 bit0、SZU bits3:0 而非 11:8)→ 真驱动判 SQS=0 拒用 CMB;② CMBMSC 跨 Controller Reset 误清(disable 清 cre/cmse/cba,但真 Linux nvme_map_cmb 仅编程一次依赖其持久)。修复 commit 3a1779c1d。**SQ-in-CMB 是 firmware 自读 backing,不需 host dma-buf/fork QEMU**。详见 `experiments/2026-06-13-cmb-l4-realmachine-qemu/findings.md` | — |

另:**P6**(map-on-OpenHCL 的 `create_ram_gpa_range` 别名推测路径,§5/§8)是独立 track 的真机 POC,与 L4 不同——L4 是"已实现的 trap/map 在真机跑通",P6 是"验证一条尚未实现的 OpenHCL 零拷贝推测路径是否可行"。

**结论**:CMB 在 L1+L2 完整;**L4 真机已完全达成**(trap+map 双模真 Linux 驱动 SQ-in-CMB PASS);L3 真二进制起服务由 L4 harness 一并覆盖。P6(map-on-OpenHCL §5)仍是独立未决 track。
