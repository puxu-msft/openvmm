# NVMe TCP Transport + Fabric Commands — Byte-Level Reference

> **📖 EVERGREEN (2026-06-06 audit)** — wire 字节级权威；Phase V / V-followup 所有 wire 实现以本文为准。新 spec 字段或 layout 修正 (V-interop-5/6 / 9 个 wire blocker 教训) **应同步更新本文**，并配 [LESSONS.md](../plans/LESSONS.md) §1 `offset_of!` anchor test。

> 由 subagent fetch Linux `nvme-tcp.h` + SPDK `nvmf_spec.h` 整合，作为
> Phase V (NVMe-oF TCP target) 实现的唯一参考。

## 1. PDU Common Header (8 bytes, LE)

```
struct CommonHdr (CH)
| 0 | 1 | pdu_type | 见 §2 |
| 1 | 1 | flags    | HDGSTF=0x01, DDGSTF=0x02, DATA_LAST=0x04, DATA_SUCCESS=0x08 |
| 2 | 1 | hlen     | CH+PSH 长度（不含 HDGST） |
| 3 | 1 | pdo      | 数据起始 byte 偏移（从 PDU 起算） |
| 4 | 4 | plen u32 | 总 PDU 字节数（含 CH+PSH+HDGST+pad+data+DDGST） |
```

- HDGST: 4-byte CRC32C(CH..PSH)，从 byte hlen 开始
- DDGST: 4-byte CRC32C(data only)，PDU 末尾
- pad: `pdo - hlen - (HDGSTF?4:0)` 字节，零填充

## 2. PDU 类型

```rust
pub const NVME_TCP_ICREQ:    u8 = 0x00; //  128 B
pub const NVME_TCP_ICRESP:   u8 = 0x01; //  128 B
pub const NVME_TCP_H2C_TERM: u8 = 0x02; //  24..152 B
pub const NVME_TCP_C2H_TERM: u8 = 0x03; //  24..152 B
pub const NVME_TCP_CMD:      u8 = 0x04; //  CapsuleCmd: 72 + ICD
pub const NVME_TCP_RSP:      u8 = 0x05; //  CapsuleResp: 24 B
pub const NVME_TCP_H2C_DATA: u8 = 0x06; //  24 + data
pub const NVME_TCP_C2H_DATA: u8 = 0x07; //  24 + data
pub const NVME_TCP_R2T:      u8 = 0x09; //  24 B
```

## 3. flags 位

```rust
pub const F_HDGST:        u8 = 1 << 0;
pub const F_DDGST:        u8 = 1 << 1;
pub const F_DATA_LAST:    u8 = 1 << 2;
pub const F_DATA_SUCCESS: u8 = 1 << 3; // C2HData only：兼当 CQE 不再发 CapsuleResp
```

## 4. 各 PDU PSH

### ICReq / ICResp (128 B)
```
| 8  | 2   | pfv       u16  | =0
| 10 | 1   | hpda/cpda u8   | dword align, 0..31, 实际 = (v+1)*4
| 11 | 1   | digest    u8   | bit0=HDGST 启用，bit1=DDGST
| 12 | 4   | maxr2t / maxh2cdata u32 |
| 16 | 112 | rsvd
```

### TermReq (24..152 B)
```
| 8  | 2   | fes  u16  | 0x01 INVALID_PDU_HDR, 0x02 PDU_SEQ_ERR, 0x03 HDR_DIGEST_ERR,
                            0x04 DATA_OUT_OF_RANGE, 0x05 R2T_LIMIT, 0x06 UNSUPP_PARAM
| 10 | 4   | fei  [u8;4]
| 14 | 10  | rsvd
| 24 | var | error_data (0..128 B)
```

### CapsuleCmd (72 B + ICD)
```
| 8  | 64  | SQE (struct nvme_command)
```
SQE: cid @ byte 2..4。ICD（in-capsule data）跟 pdo 起；admin queue 上限 8 KiB。

### CapsuleResp (24 B)
```
| 8  | 16  | CQE { result u32, rsvd u32, sq_head u16, sq_id u16, command_id u16, status u16 }
```

### R2T (24 B)
```
| 8  | 2 | cccid       u16  | 对应 H2C cmd 的 command_id
| 10 | 2 | ttag        u16  | controller 选的 transfer tag
| 12 | 4 | r2t_offset  u32  | cmd buffer 内 byte offset
| 16 | 4 | r2t_length  u32  | 准许 host 现在发的字节数
| 20 | 4 | rsvd
```

### H2CData (24 + data)
```
| 8  | 2 | cccid       u16
| 10 | 2 | ttag        u16  | 必须匹配 R2T.ttag
| 12 | 4 | data_offset u32
| 16 | 4 | data_length u32  | ≤ ICResp.maxh2cdata
| 20 | 4 | rsvd
```
末段设 F_DATA_LAST。

### C2HData (24 + data)
```
| 8  | 2 | cccid       u16
| 10 | 2 | rsvd        (无 ttag)
| 12 | 4 | data_offset u32
| 16 | 4 | data_length u32
| 20 | 4 | rsvd
```
末段设 F_DATA_LAST；若同时 F_DATA_SUCCESS，省 CapsuleResp。

## 5. Digest

CRC32C Castagnoli（0x1EDC6F41）；init 0xFFFFFFFF；final XOR 0xFFFFFFFF；
reflected I/O；4 byte LE。

## 6. 连接流程

1. TCP accept (port 4420 IO / 8009 discovery)
2. ICReq → ICResp，固化 digest/align/maxh2cdata
3. CapsuleCmd 带 Fabrics Connect (admin qid=0)
4. Property Set/Get 配 CC，再走标准 NVMe admin

## 7. Fabric Commands (opcode 0x7F)

SQE byte 4 = fctype：
```rust
pub const FCTYPE_PROPERTY_SET: u8 = 0x00;
pub const FCTYPE_CONNECT:      u8 = 0x01;
pub const FCTYPE_PROPERTY_GET: u8 = 0x04;
pub const FCTYPE_AUTH_SEND:    u8 = 0x05;
pub const FCTYPE_AUTH_RECV:    u8 = 0x06;
pub const FCTYPE_DISCONNECT:   u8 = 0x08;
```

### Connect SQE (64 B) + Connect Data (1024 B)

SQE：
```
| 24 | 16 | sgl1 NVMe SGL descriptor 描述 Connect Data
| 40 | 2  | recfmt u16  | =0
| 42 | 2  | qid    u16  | 0=admin
| 44 | 2  | sqsize u16  | 0-based
| 46 | 1  | cattr  u8
| 47 | 1  | rsvd
| 48 | 4  | kato   u32  | KeepAlive ms, admin only
```

Data **1024 B**（修正 plan 中 1792 错误）：
```
| 0    | 16  | hostid    [u8;16]   (UUID raw bytes)
| 16   | 2   | cntlid    u16       (0xFFFF 动态, 0xFFFE static-any, 其它指定)
| 18   | 238 | rsvd
| 256  | 256 | subsysnqn ASCII zero-padded
| 512  | 256 | hostnqn   ASCII zero-padded
| 768  | 256 | rsvd
```

Connect CQE：
- 成功：DW0 bits 0..15 = 分配的 cntlid，bits 16..31 = authreq 等
- 失败：DW0 bits 0..15 = ipo (invalid parameter offset, dword 单位)，bits 16..23 = iattr (bit0: 0=command 1=data)

Common fabric SC: 0x80 INCOMPATIBLE_FORMAT, 0x81 CONTROLLER_BUSY,
0x82 CONNECT_INVALID_PARAM, 0x84 CONNECT_INVALID_HOST。

### Property Get/Set (no data)

```
| 40 | 1 | attrib u8 | bits 0..2: size (0=4B, 1=8B)
| 41 | 3 | rsvd
| 44 | 4 | ofst   u32 | 0x00 CAP, 0x08 VS, 0x14 CC, 0x1C CSTS, 0x20 NSSR
| 48 | 8 | value  u64 LE (Set only)
| 56 | 8 | rsvd
```

Get reply: CQE.result DW0 = 低 4B value；8B 模式低 DW0 + 高 DW1。

### Disconnect (fctype=0x08)
SQE 标准 64B + `recfmt u16 @ 40`；无 payload；拆队列后 host close TCP。
