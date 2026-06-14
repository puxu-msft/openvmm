# static controller model conformance — 设计 plan (2026-06-14)

> **状态**：execution-ready。源于母项目 Windows NVMe-oF 互通手册暴露的盲区
> （`experiments/2026-06-13-openhcl-vpci-nvme/`）+ probe 实测坐实（`tests/vt_static_controller_model_probe.rs`）。
> ROADMAP §1 `V-followup-static-controller-model`。

## 1. 问题（已实测坐实）

usnvmemu NVMe-oF target 是 **dynamic-controller-model-only**：
- Discovery Log 写死 `cntlid: 0xFFFF`（`nvme_firmware/src/controller/discovery_log.rs:173`）。
- `handle_connect_async`（`async_session.rs:634-774`）+ `handle_connect`（sync `session.rs:987-1068`）
  解析 `ConnectData` 但**从不读 `cd.cntlid`**，无条件返 `TEACHING_CNTLID=1`。
- probe 实测：host 请求 CNTLID=5 → SC=0 + 分配 1（**静默 coerce，无错误**），spec 偏离。

Windows inbox initiator 默认 static controller model → 这条路 Linux nvme-cli（默认 dynamic）测不到。

## 2. spec 锚定（源：wire-reference `docs/specs/2026-06-04-nvme-tcp-wire-reference.md:146,154-155`）

Connect data offset 16 = `cntlid u16`：
- `0xFFFF` = dynamic（host 不指定，subsystem 任分）
- `0xFFFE` = static controller model「任一可用 static controller」
- 其它 = static 指定**具体** controller id

Connect 响应 CQE DW0：
- **成功**：bits 0..15 = 分配的 cntlid，bits 16..31 = authreq 等。
- **失败**：bits 0..15 = **IPO**（invalid parameter offset，**dword 单位**），bits 16..23 = **IATTR**
  （bit0：0=command / 1=data）。

`fabric_sc::CONNECT_INVALID_PARAM = 0x82`（`fabric.rs:258`，已存在）。

## 3. 设计

### Part A — Connect cntlid 验证（dual-track via dispatch_plan 纯函数）

usnvmemu 是**单 controller**（cntlid=1）target。把「host 请求的 cntlid 解析到哪个 controller」抽成
纯函数，sync/async 共用（防漂移，对齐既有 `decide_*` 模式）：

```rust
// dispatch_plan.rs
pub enum ConnectCntlidDecision {
    Accept(u16),          // 解析成功 → 分配此 cntlid
    RejectInvalidParam,   // static 指定一个我们没有的 controller
}
pub fn decide_connect_cntlid(requested: u16, our_cntlid: u16) -> ConnectCntlidDecision {
    match requested {
        0xFFFF => Accept(our_cntlid),                     // dynamic
        0xFFFE => Accept(our_cntlid),                     // static-any
        0 => Accept(our_cntlid),                          // 未指定/legacy（见下 lenience）
        x if x == our_cntlid => Accept(our_cntlid),       // static 具体匹配
        _ => RejectInvalidParam,                          // static 具体 != 我们的
    }
}
```

**permissive 设计理由**：单 controller target，凡能解析到「controller 1」的请求都接受（dynamic /
static-any / specific==1），只 reject「指定一个不存在的具体 cntlid」（如 probe 的 5）。一个 conformant
host 永远按 discovery 广告的 model 发请求，不会送 mismatch；permissive 不违反任何 conformant host 会
测的行为，而 reject specific≠1 正是 conformance 修复点。

**`cntlid=0` 的 lenience（刻意，文档化）**：真实 host **admin connect 永不送 0**（Linux nvme-tcp 送
`0xFFFF` dynamic；Windows static 送 discovery 学到的具体值或 `0xFFFE`）。但本 crate 21 个既有测试 fixture
用 `ConnectData::default()`（cntlid=0），是 cntlid-awareness 之前的 legacy 笔法。把 0 视作「未指定→
accept」**对任何 conformant host 路径零影响**（与 architect 背书的 permissive 同一论证：no conformant
host 送这种请求），且零 fixture churn（避免 21-file 共享树 sweep 的事故风险）。spec-strict reject-0 见
下方 deferred。**architect spec-audit 已 PASS 本 permissive 取舍。**

**deferred spec-strict（基础/高级可拆，本 phase 不做，记 ROADMAP）**：
- ① reject `cntlid=0`（严格：0 是具体 controller id，不存在应 0x82）+ 把 21 个 fixture 改送 faithful
  `0xFFFF`（admin）/ `1`（IO），使全测试套对齐真实 Linux dynamic 行为。
- ② IO queue（qid≥1）Connect 拒哨值（`0xFFFF`/`0xFFFE`）——spec 要求 IO connect 送 admin 已分配的
  **具体** cntlid（architect MEDIUM）。本 phase `decide_connect_cntlid` 不带 qid 维度，admin/IO 同处理。
两项都非 Windows-static-interop 承重路径（real host 不触发），是 spec-strictness 纯化，留 `V-spec-strict-mode`。

**接入点**（两处，验证放 qid 分支**之前**，对 admin qid=0 与 IO qid≥1 都生效——IO connect 的 cntlid
须 == admin 分配的，static host 会送 1，`decide_connect_cntlid(1,1)=Accept`）：
- `async_session.rs handle_connect_async`：parse `cd` 后立即 `decide_connect_cntlid(cd.cntlid, self.cntlid)`；
  `Accept(c)` → 继续（assigned=c）；`RejectInvalidParam` → 发 err（见 Part B）early return，**不**置
  `admin_connected` / **不** arm KATO。
- `session.rs handle_connect`：同。

### Part B — err 响应携带 IPO/IATTR（spec-complete reject）+ 修 SCT（CRITICAL）

**B0 — SCT 修复（CRITICAL，architect spec-audit 发现，承重）**：现 err helper（`async_session.rs:923` +
`session.rs:1207`）对 SC `0x80..=0x9F` 一律用 **SCT=0x07（Vendor Specific）**——**这是既存 wire bug**。
NVMe-oF fabrics Connect 状态码（0x80–0x84）属 **SCT=0x01（Command Specific Status）**，IPO/IATTR 的
Dword0 语义**只在 SCT=0x01 下定义**。SCT=0x07 ⟹ conformant host（含 Windows）把 0x82 解读成厂商私有
错误而非「Connect Invalid Parameters」，**本 plan 精心算的 IPO/IATTR 形同虚设**。根因：原注释
`session.rs:1205` 把 0x07 误标为「Command Specific」（实为 Vendor Specific），且测试 `session.rs:1843`
`assert_eq!(sct, 0x07)` 把错值锁死（self-consistent 陷阱：production+test 互洽两边都错）。
- 修：`0x80..=0x9F → 0x01`（async + sync 两处 helper），更新注释，改 `session.rs:1843` 断言为 `0x01`。
- 它没早暴露，是因既有测试只查「connect 二元失败」非「host 精确解码 SC+IPO」；本 plan 的 IPO 测试 +
  真 host 目标使它承重。Linux 真互通不回归（成功路径 SC=0；错误路径 Linux 任何非零 status 都 fail connect）。

**B1 — err result_dw0 变体**：现 `send_capsule_resp_err{_async}(cid, sc)` 把 result_dw0 清 0。加变体携带：

```rust
// async + sync 各一；旧 err helper 改为转调新变体 result_dw0=0（DRY）
fn send_capsule_resp_err_with_result[_async](cid, sc, result_dw0) { cqe[0..4]=result_dw0; ... }
```

cntlid reject 的 result_dw0：IATTR.bit0=1（data）⟹ **IPO 以 Connect data payload 起点为基准**；cntlid
在 data offset 16 → IPO = 16/4 = **dword 4**：

```rust
// fabric.rs
// IATTR.bit0=1 ⟹ IPO 相对 Connect data payload 起点。此处依赖 ConnectData struct
// 首字节(hostid)即 data payload byte 0，故 offset_of! 即 data-relative 偏移。
const _: () = assert!(core::mem::offset_of!(ConnectData, hostid) == 0); // 锚死 data-relative 前提
pub const CONNECT_CNTLID_INVALID_RESULT_DW0: u32 =
    (core::mem::offset_of!(ConnectData, cntlid) as u32 / 4)   // IPO dword (= 4)
    | (1u32 << 16);                                           // IATTR bit0=data
// = 0x0001_0004
```

> **不**回填其它已存在的 Connect error（INVALID_HOST 等）的 IPO——scope 限定本 cntlid 路径；
> 其它 error 的 IPO 完整化留独立项（注释标注）。SCT 修复则惠及全部 fabrics error（顺带正确）。

### Part C — Discovery static-model 广告（CLI flag）

让 static-default host 经 discovery 学到具体 cntlid（而非 0xFFFF dynamic 哨值）：
- `discovery_log.rs DiscoveryPortal` 加字段 `cntlid: u16`（default `0xFFFF`）；`build_discovery_log`
  用 `p.cntlid` 替死写的 `0xFFFF`（`from_ipv4_addr` 默认填 `0xFFFF` 保持现行为）。
- bin 加 flag `--discovery-static-cntlid <u16>`（或 `--controller-model static`）：discovery_mode 下把
  portal 的 cntlid 设为该值（教学默认值 = `TEACHING_CNTLID=1`）。**默认 dynamic（0xFFFF）不回归**
  现有 Linux interop。

## 4. 测试（TDD，已有 probe 作起点）

- **dispatch_plan 单测**：`decide_connect_cntlid` 全分支（0xFFFF/0xFFFE/specific==1/specific!=1）。
- **probe 改造**（`vt_static_controller_model_probe.rs`）：
  - `static_specific_request_is_silently_coerced_to_1` → **改名** `static_specific_request_rejected_invalid_param`，
    断言 SC=0x82 + CQE result_dw0 == `0x0001_0004`（IPO=4/IATTR=data）。先 RED（现 coerce）再 GREEN。
  - 保留 dynamic(0xFFFF)→accept→1、static-any(0xFFFE)→accept→1、新增 specific==1→accept→1。
- **sync 单测**（session.rs）：sync `handle_connect` 对 specific!=1 reject（dual-track 不漏）。
- **discovery 静态广告测**（discovery_log.rs）：portal cntlid=1 时 entry offset 6..8 == `0x0001`；
  default portal 仍 `0xFFFF`。
- **回归**：现有 v_interop_2（dynamic IO connect）+ session.rs Connect 测全绿不动。

## 5. 验收

- probe 4 test 绿（含 reject + IPO/IATTR 断言）；dispatch_plan 单测绿；sync 单测绿；discovery 测绿。
- `cargo test -p nvme_of_tcp_target`（全量，含 nvme_firmware discovery_log 测）全绿，clippy 0 warning。
- `#![forbid(unsafe_code)]` 维持（offset_of! 是 safe）。
- ROADMAP 标 in-progress→done；wire-reference 若需补 IPO 示例则同步；companion 更新「已修复」。

## 6. commit 切分（语义单元）

- C1：Part A+B（Connect cntlid conformance + dispatch_plan 纯函数 + IPO/IATTR err + 测试）。
- C2：Part C（discovery static-model 广告 + flag + 测试 + 文档）。
（两者独立语义单元；C1 是 standalone 的 conformance 修复，C2 是 interop 完整化。）
