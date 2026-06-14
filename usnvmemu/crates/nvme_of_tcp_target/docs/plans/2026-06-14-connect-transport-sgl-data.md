# Fix B — NVMe-oF Connect: 支持 transport-SGL Connect data（R2T fetch）

> **状态**：✅ **SHIPPED（2026-06-14，真 WS2025 出盘 PROVEN）**。源于 **真 WS2025 inbox initiator 真机互通**实测：
> Windows IO-queue Fabric Connect 用 **Transport SGL（type 0x5）** 推送 1024B Connect data
> （非 in-capsule），但本 target 只认 in-capsule（`data.len()==1024`），rejected SC=0x82 →
> IO 队列建不起来 → namespace 不出盘。Linux 总是 in-capsule 所以从没暴露。
> 修复后真 WS2025 `Get-Disk` 出盘 Online + 4KB byte-equal IO；3 集成测试固化 Windows wire；
> architect PASS-with-fixes（timeout 已补）+ rust-reviewer APPROVE。

## 1. 真机实测根因（byte-level 坐实）

WS2025 `nvmeofutil connect` 经 relay 真连 usnvmemu target，relay byte-log 解码：

| conn | CapsuleCmd | SGL1 type byte (sqe[39]) | SGL len (sqe[32..36]) | in-capsule | 结果 |
|---|---|---|---|---|---|
| admin (qid=0) | plen=1096 = 72 + **1024 data** | `0x01`（Data Block, subtype 1 = in-capsule offset） | 1024 | 是 | ✅ accept |
| IO (qid=1) | plen=72 = SQE only，**无 data** | `0x5A`（**Transport SGL Data Block**, subtype 0xA） | 1024 | 否 | ❌ SC=0x82 |

根因链：[cmd.rs:760](/usnvmemu/crates/nvme_firmware/src/cmd.rs) `IOCCSZ=4`（IO 命令胶囊=64B SQE，**不支持 in-capsule data**）→ Windows 正确地对 IO Connect data 用 transport SGL（R2T/H2CData）→ 但 [async_session.rs:629](/usnvmemu/crates/nvme_of_tcp_target/src/async_session.rs) `if data.len() != 1024 { reject }` 在 parse 前就拒，**从不发 Windows 等的 R2T**。**内部不一致**：target 广告「IO 无 in-capsule」却要求 Connect data in-capsule。

## 2. spec 锚定（架构 review 重点核）

NVMe SGL Descriptor，SGL Identifier byte（descriptor byte 15）高 nibble = type：
- `0x0` = **SGL Data Block descriptor**；NVMe-oF 用 subtype `0x1`（in-capsule，offset）→ data 紧跟 SQE。
- `0x5` = **Transport SGL Data Block descriptor**；NVMe-oF/TCP 用 subtype `0xA`（transport-specific）→
  data **经 transport 传**（host→controller = controller 发 R2T，host 回 H2CData）。

SGL Data Block descriptor 16-byte 布局：byte 0-7 = Address，**byte 8-11 = Length（u32 LE）**，
byte 12-14 = rsvd，**byte 15 = SGL Identifier**（高 nibble type / 低 nibble subtype）。Connect SQE
的 SGL1 在 **SQE byte 24..40**（DPTR 字段）。

> **承重假设（架构核）**：「type 0x5 = data 经 transport（R2T）/ type 0x0 = in-capsule」是否 spec
> 正确?这是 Fix 的判据，错则整个路由错。

## 3. 设计（通用 SGL 路由，admin + IO 都覆盖）

### 3a — `fabric.rs`：纯函数 `decode_connect_data_source`
```rust
/// SGL Identifier type nibble（descriptor byte 15 高 4 位）。
pub mod sgl_type {
    pub const DATA_BLOCK: u8 = 0x0;            // NVMe-oF in-capsule（subtype 0x1）
    pub const TRANSPORT_DATA_BLOCK: u8 = 0x5;  // 经 transport（R2T/H2CData）
}

pub enum ConnectDataSource {
    InCapsule,            // data 已随 capsule 到达（Linux + 任意 in-capsule）
    ViaR2t { len: u32 },  // transport SGL — 须发 R2T 取（Windows IO Connect）
    Invalid,              // 既非合法 in-capsule 也非合法 transport → reject SC=0x82
}

/// 据 Connect SQE 的 SGL1 + 已到达的 in-capsule 字节数，判 Connect data 怎么取。
pub fn decode_connect_data_source(sqe: &[u8], in_capsule_len: usize) -> ConnectDataSource {
    if in_capsule_len == CONNECT_DATA_SIZE {
        return ConnectDataSource::InCapsule; // 兼容 Linux/admin in-capsule（SGL 不必再看）
    }
    if sqe.len() < 64 || in_capsule_len != 0 {
        return ConnectDataSource::Invalid; // 部分 in-capsule = 非法
    }
    let sgl_type = (sqe[39] >> 4) & 0x0F;
    let sgl_len = u32::from_le_bytes(sqe[32..36].try_into().unwrap());
    if sgl_type == sgl_type::TRANSPORT_DATA_BLOCK && sgl_len == CONNECT_DATA_SIZE as u32 {
        ConnectDataSource::ViaR2t { len: sgl_len }
    } else {
        ConnectDataSource::Invalid
    }
}
```

### 3b — `handle_connect_async`（async）接入
把开头 `if data.len() != CONNECT_DATA_SIZE { reject }` 改为：
```rust
let connect_data: Vec<u8> = match fabric::decode_connect_data_source(sqe, data.len()) {
    ConnectDataSource::InCapsule => data.to_vec(),
    ConnectDataSource::ViaR2t { len } => {
        // Windows IO Connect：发 R2T 取 1024B Connect data（复用现成三段式机件）
        self.dma_read_via_r2t_async(cid, 0, len).await
            .context("Connect data via R2T")?
    }
    ConnectDataSource::Invalid => {
        return self.send_capsule_resp_err_async(cid, fabric_sc::CONNECT_INVALID_PARAM).await;
    }
};
let cd = match ConnectData::read_from_bytes(&connect_data) { ... };
// 其余（hostnqn / cntlid 校验 / qid 分支 / CapsuleResp）完全不变，用 cd。
```
顺序正确：host 送 Connect(transport SGL) → 我们发 R2T → host 回 H2CData(1024) → parse + 校验 →
CapsuleResp（success / cntlid reject）。`dma_read_via_r2t_async` 已被 fused-CAS 在 dispatch 内调用，
nested-read H2CData 安全（established pattern）。

### 3c — `handle_connect`（sync session.rs）对称
同样改，用 sync `dma_read_via_r2t(cid, 0, len)`（session.rs:903）。dual-track 一致（虽 Windows 只走
async；sync 是 legacy/test 路径，保持一致 + 不回归）。

## 4. 测试

- **fabric 单测** `decode_connect_data_source`：in-capsule(1024)→InCapsule；transport SGL(type0x5,len1024,
  in_capsule=0)→ViaR2t；部分/其它→Invalid。用真 Windows SGL byte（`...5a` @ sqe[39]，len@32..36）。
- **集成测试**（模拟 Windows IO Connect）：craft IO Connect（qid=1，**无 in-capsule data**，SGL1 type=0x5A
  len=1024）→ 期望 target **发 R2T**(cccid=cid) → 测试回 H2CData(1024B Connect data) → 期望 CapsuleResp
  SC=0 + IO queue 建立。这是把真 Windows 行为固化的 regression gate。
- **回归**：admin Connect in-capsule（c2/Linux）+ 现有 v_interop_2 IO Connect in-capsule 全绿不变。
- **真机**：rebuild target → WS2025 重连 → 期望 IO 队列建立 + Identify Namespace + **Get-Disk 出 NVMeof 盘** + 读写。

## 5. 验收
- fabric 单测 + 新集成测试绿；全套 + clippy 绿；`#![forbid(unsafe_code)]` 维持。
- 真机 WS2025 出盘（Get-PhysicalDisk BusType=NVMeof）+ IO 读写通。
- architect spec-check PASS（§2 SGL type 语义）；rust-reviewer APPROVE。

## 6. commit（语义单元）
单 commit：Connect transport-SGL data 支持（fabric 路由 + dual-track 接入 + 测试）。真机出盘截图/日志
记 companion + memory。
