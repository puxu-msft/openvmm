# vfio-user 协议字节级参考（自调研 + libvfio-user header 验证）

> 由 general-purpose subagent 跨 fetch 三份权威源（libvfio-user `vfio-user.h` /
> spec `.rst` / Linux `vfio.h`）后整合，作为 Phase U 实现的唯一参考。

## 1. Common Header（16 字节，host-endian = LE x86）

```rust
#[repr(C, packed)]
struct VfioUserHeader {
    msg_id:   u16,  // off  0
    cmd:      u16,  // off  2
    msg_size: u32,  // off  4   total bytes incl. header
    flags:    u32,  // off  8
    error_no: u32,  // off 12   UNIX errno on reply; 0 on cmd or success
}
```

**flags 位**（与最初草图 *不一致*）：

| Bits | 名字 | 含义 |
|------|------|------|
| 0-3  | `F_TYPE_MASK` (0x0f) | 0=COMMAND, 1=REPLY |
| 4    | `F_NO_REPLY`  (0x10) | command only |
| 5    | `F_ERROR`     (0x20) | reply only：payload 为 0，errno 在 off 12 字段 |
| 6-31 | reserved | 0 |

⚠️ 之前 plan 写"bits 28-31 是 error code" **是错的**——errno 走 `error_no` 字段。

## 2. 命令号（authoritative，从 vfio-user.h）

```
VFIO_USER_VERSION                    = 1   C→S
VFIO_USER_DMA_MAP                    = 2   C→S
VFIO_USER_DMA_UNMAP                  = 3   C→S
VFIO_USER_DEVICE_GET_INFO            = 4   C→S
VFIO_USER_DEVICE_GET_REGION_INFO     = 5   C→S
VFIO_USER_DEVICE_GET_REGION_IO_FDS   = 6   C→S   (不需实现)
VFIO_USER_DEVICE_GET_IRQ_INFO        = 7   C→S
VFIO_USER_DEVICE_SET_IRQS            = 8   C→S
VFIO_USER_REGION_READ                = 9   C→S
VFIO_USER_REGION_WRITE               = 10  C→S
VFIO_USER_DMA_READ                   = 11  S→C   server-initiated
VFIO_USER_DMA_WRITE                  = 12  S→C   server-initiated
VFIO_USER_DEVICE_RESET               = 13  C→S
VFIO_USER_REGION_WRITE_MULTI         = 15  (optional)
VFIO_USER_DEVICE_FEATURE             = 16  (migration; defer)
VFIO_USER_MIG_DATA_READ              = 17  (defer)
VFIO_USER_MIG_DATA_WRITE             = 18  (defer)
```

⚠️ 之前 plan 写的 6-12 与实际 7-13 全错（GET_REGION_IO_FDS=6 插队）。

## 3. 每个命令的 payload

### VERSION (1)
```rust
#[repr(C, packed)]
struct VfioUserVersion { major: u16, minor: u16 }
// 后跟 UTF-8 JSON capability blob，NUL 终止
```
当前 protocol：major=0, minor=1。

### DMA_MAP (2)
```rust
#[repr(C, packed)]
struct VfioUserDmaMap {
    argsz:  u32, flags: u32,   // flags bit0=READABLE bit1=WRITEABLE
    offset: u64,               // fd 内 offset
    addr:   u64,               // IOVA
    size:   u64,
}
```
若 fd 不发，server **必须** 改用 DMA_READ/WRITE 访问该范围。
Server 完全可拒所有 fd（直接忽略收到的 fd），强制 message-mediated DMA。

### DMA_UNMAP (3)
```rust
#[repr(C, packed)]
struct VfioUserDmaUnmap { argsz: u32, flags: u32, addr: u64, size: u64 }
```
`flags` bit0=GET_DIRTY_BITMAP, bit1=UNMAP_ALL。Reply 必须 echo 同 struct。

### DEVICE_GET_INFO (4)
```rust
#[repr(C, packed)]
struct VfioUserDeviceInfo {
    argsz: u32, flags: u32,    // bit0=RESET, bit1=PCI
    num_regions: u32,          // 9
    num_irqs:    u32,          // 5
}
```
我们 reply：`flags=0x3, num_regions=9, num_irqs=5`。

### DEVICE_GET_REGION_INFO (5)
```rust
#[repr(C, packed)]
struct VfioUserRegionInfo {
    argsz: u32, flags: u32,    // bit0=READ bit1=WRITE bit2=MMAP bit3=CAPS
    index: u32, cap_offset: u32,
    size: u64, offset: u64,
}
```
Region index：BAR0-5=0..5, ROM=6, CONFIG=7, VGA=8, num=9。
NVMe BAR0（8 KiB MMIO，**不要 mmap** → 所有 reg 访问走 REGION_READ/WRITE）：
- `index=0, flags=0x3 (RD|WR), size=0x2000, offset=0, cap_offset=0`

CONFIG (7)：`flags=0x3, size=0x1000`。
其它 BAR1-5/ROM/VGA：`flags=0, size=0`。

### DEVICE_GET_IRQ_INFO (7)
```rust
#[repr(C, packed)]
struct VfioUserIrqInfo {
    argsz: u32, flags: u32,    // bit0=EVENTFD bit1=MASKABLE bit2=AUTOMASKED bit3=NORESIZE
    index: u32, count: u32,
}
```
MSI-X (index=2)：`flags=EVENTFD, count=<msix_count>`。
其它 INTX/MSI/ERR/REQ：`count=0`。

### DEVICE_SET_IRQS (8)
```rust
#[repr(C, packed)]
struct VfioUserIrqSet {
    argsz: u32, flags: u32, index: u32, start: u32, count: u32,
    // 后跟 data 视 flags 而定
}
```
`flags` bits：
| Bit | 名 | 效果 |
|---|---|---|
| 0 | DATA_NONE   (0x01) | 无 data |
| 1 | DATA_BOOL   (0x02) | data = count bool |
| 2 | DATA_EVENTFD(0x04) | data 空；count 个 fd 经 SCM_RIGHTS |
| 3 | ACTION_MASK   (0x08) | |
| 4 | ACTION_UNMASK (0x10) | |
| 5 | ACTION_TRIGGER(0x20) | (de)assign / trigger |

QEMU 典型：`flags=0x24 (EVENTFD|TRIGGER), index=2, count=N` + N fd。
Server 触发 MSI-X 向量：往 stored eventfd 写 8 字节 `u64=1`。

### REGION_READ (9) / WRITE (10)
```rust
#[repr(C, packed)]
struct VfioUserRegionAccess { offset: u64, region: u32, count: u32 }
```
Request: 无 data（READ）/ data[count] 跟随（WRITE）。
Reply: 同 struct（WRITE）/ struct + data[count]（READ）。

### DMA_READ (11) / DMA_WRITE (12) — Server-Initiated
```rust
#[repr(C, packed)]
struct VfioUserDmaRwHdr { addr: u64, count: u64 }
```
DMA_READ：req=hdr only，reply=hdr+data。
DMA_WRITE：req=hdr+data，reply=hdr only。

### DEVICE_RESET (13)
无 payload，只有 16-byte header。

## 4. VERSION JSON capabilities

```json
{
  "capabilities": {
    "max_msg_fds": 8,
    "max_data_xfer_size": 1048576,
    "max_dma_maps": 65535,
    "pgsizes": 4096
  }
}
```
| key | 默认 | 备注 |
|---|---|---|
| `max_msg_fds` | **1** | 要支持 N-vector MSI-X 单 message，必须 advertise ≥ N |
| `max_data_xfer_size` | 1 MiB | DMA/REGION R/W 单次上限 |
| `max_dma_maps` | 65535 | |
| `pgsizes` | 4096 | OR'd page sizes |
| `twin_socket` | absent | 双 socket，可拒（reply supported=false）|
| `write_multiple` | absent | 不实现 |
| `migration` | omitted | 不实现 → QEMU 视为非可迁移 |

⚠️ `max_msg_fds` 默认 1，必须显式抬。

## 5. SCM_RIGHTS fd 路径

| Msg | 方向 | fd |
|---|---|---|
| DMA_MAP | C→S | 0 or 1（mmap 模式才发） |
| GET_REGION_INFO reply | S→C | 1（FLAG_MMAP 时） |
| SET_IRQS DATA_EVENTFD | C→S | count 个 |
| VERSION reply | S→C | 1（twin_socket 时） |

fd 必须随消息一起经 `sendmsg(2)/recvmsg(2)` 的 ancillary data。

## 6. Server msg_id 约定

spec 明文："Message IDs belong entirely to the sender, can be re-used"。
即两方向 id space 完全独立，无强制约定。建议 server-initiated id 顶位置 1
（如 `0x8000`+）便于日志区分，但非协议要求。

## 7. 错误处理

设置 flags bit 5 (`F_ERROR=0x20`) + `error_no` 字段填 UNIX errno + payload 空。
常见：EINVAL/EEXIST/ENOTSUP/ENODEV/EFAULT/EIO。

## 8. QEMU vfio-user-pci 启动序列

1. VERSION 交换
2. DEVICE_GET_INFO × 1
3. DEVICE_GET_REGION_INFO × 9
4. DEVICE_GET_IRQ_INFO × 5
5. DEVICE_RESET × 1
6. DMA_MAP × N（每 RAM slot）
7. SET_IRQS × 2（EVENTFD|TRIGGER 然后可能 MASK/UNMASK）
8. 进入正常 REGION_R/W + 我们偶尔 fire eventfd

## 9. 关键常量表（Rust）

```rust
const CMD_VERSION:                  u16 = 1;
const CMD_DMA_MAP:                  u16 = 2;
const CMD_DMA_UNMAP:                u16 = 3;
const CMD_DEVICE_GET_INFO:          u16 = 4;
const CMD_DEVICE_GET_REGION_INFO:   u16 = 5;
const CMD_DEVICE_GET_REGION_IO_FDS: u16 = 6;
const CMD_DEVICE_GET_IRQ_INFO:      u16 = 7;
const CMD_DEVICE_SET_IRQS:          u16 = 8;
const CMD_REGION_READ:              u16 = 9;
const CMD_REGION_WRITE:             u16 = 10;
const CMD_DMA_READ:                 u16 = 11;  // S→C
const CMD_DMA_WRITE:                u16 = 12;  // S→C
const CMD_DEVICE_RESET:             u16 = 13;

const F_TYPE_MASK:    u32 = 0x0f;
const F_TYPE_COMMAND: u32 = 0x00;
const F_TYPE_REPLY:   u32 = 0x01;
const F_NO_REPLY:     u32 = 0x10;
const F_ERROR:        u32 = 0x20;

// PCI region/irq
const PCI_NUM_REGIONS: u32 = 9;
const PCI_NUM_IRQS:    u32 = 5;
const PCI_BAR0: u32 = 0;
const PCI_CONFIG: u32 = 7;
const PCI_MSIX: u32 = 2;

const REGION_FLAG_READ:  u32 = 0x1;
const REGION_FLAG_WRITE: u32 = 0x2;
const REGION_FLAG_MMAP:  u32 = 0x4;
const REGION_FLAG_CAPS:  u32 = 0x8;

const IRQ_SET_DATA_NONE:      u32 = 0x01;
const IRQ_SET_DATA_BOOL:      u32 = 0x02;
const IRQ_SET_DATA_EVENTFD:   u32 = 0x04;
const IRQ_SET_ACTION_MASK:    u32 = 0x08;
const IRQ_SET_ACTION_UNMASK:  u32 = 0x10;
const IRQ_SET_ACTION_TRIGGER: u32 = 0x20;

const IRQ_INFO_EVENTFD:    u32 = 0x1;

const DEVICE_FLAGS_RESET: u32 = 0x1;
const DEVICE_FLAGS_PCI:   u32 = 0x2;
```
