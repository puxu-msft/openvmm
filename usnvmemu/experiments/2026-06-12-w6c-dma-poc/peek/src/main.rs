//! W6c POC：从 VTL2 内一个**独立进程** mmap `/dev/mshv_vtl_low` 到任意 guest GPA，
//! 读出该处的活 guest RAM。
//!
//! 目的：验证 finding-④ 修复（策略 A，underhill `GuestMemory::sharing()` → DMA_MAP）的
//! 承重残留假设 #1——**高 GPA 的跨进程 mmap 能否返回真 guest 内存**。W5a 已在低 GPA
//! (0x100000) 证过这条机制（mmap + firmware 零拷贝 DMA），但没测过高 GPA。本 POC 直接
//! 打 usnvmemu 当前 DMA 失败的那个 GPA（admin SQ，guest 已写入 Identify SQE 但控制器
//! 因无 DMA_MAP 读不到），若能读出一条合法 NVMe SQE，即证：高 GPA 可 mmap + 数据就在
//! 那里 + 唯一缺的就是 DMA_MAP 接线。
//!
//! 用法：`w6c_peek <gpa-hex> [len]`（默认 len=64=一条 admin SQE）。
//! file_offset = GPA（非-CVM/非隔离，alias-map off；见 mapping.rs 线性 offset 公式）。

use nix::sys::mman::{MapFlags, ProtFlags, mmap};
use std::num::NonZeroUsize;

fn parse_u64(s: &str) -> u64 {
    let t = s.trim().trim_start_matches("0x");
    u64::from_str_radix(t, 16).unwrap_or_else(|_| s.trim().parse().unwrap_or(0))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let gpa = args.get(1).map(|s| parse_u64(s)).unwrap_or(0x100000);
    let len = args.get(2).and_then(|s| s.parse::<usize>().ok()).unwrap_or(64);

    const PAGE: u64 = 4096;
    let aligned = gpa & !(PAGE - 1);
    let delta = (gpa - aligned) as usize;
    let map_len = (((delta + len) as u64 + PAGE - 1) & !(PAGE - 1)).max(PAGE);

    let dev = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/mshv_vtl_low")?;
    let nz = NonZeroUsize::new(map_len as usize).unwrap();
    // SAFETY: mmap 真 guest RAM 设备 fd，file_offset=对齐后的 GPA（非-CVM 裸 GPA，
    // 见 W5a POC-3 + mapping.rs:611/647 线性 offset）。PROT_READ + MAP_SHARED，本进程独占映射。
    let addr = unsafe {
        mmap(
            None,
            nz,
            ProtFlags::PROT_READ,
            MapFlags::MAP_SHARED,
            &dev,
            aligned as i64,
        )?
    };
    let base = addr.as_ptr() as *const u8;
    // SAFETY: addr 为 mmap 成功返回的 map_len 字节有效映射，读 [delta, delta+len) 在界内。
    let data = unsafe { std::slice::from_raw_parts(base.add(delta), len) };

    eprintln!("[peek] gpa={gpa:#x} len={len} (mmap_base={aligned:#x} delta={delta} map_len={map_len:#x})");
    for (i, chunk) in data.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("  {:#010x}: {}", gpa as usize + i * 16, hex.join(" "));
    }
    let nonzero = data.iter().any(|&b| b != 0);
    let all_ff = data.iter().all(|&b| b == 0xff);
    // NVMe admin SQE：byte0 = opcode（Identify=0x06）；CID 在 byte2..4；PRP1 在 byte24..32。
    let opcode = data.first().copied().unwrap_or(0);
    let prp1 = if len >= 32 {
        u64::from_le_bytes(data[24..32].try_into().unwrap())
    } else {
        0
    };
    eprintln!(
        "[peek] opcode(byte0)={opcode:#04x} prp1={prp1:#x} nonzero={nonzero} all_ff={all_ff}"
    );
    if opcode == 0x06 {
        eprintln!("[peek] VERDICT: byte0=0x06 = NVMe Identify —— 高 GPA mmap 读出活 guest SQE，承重假设 #1 PROVEN");
    } else if nonzero && !all_ff {
        eprintln!("[peek] VERDICT: 非零非全-FF 的真实 guest 数据 —— 高 GPA mmap 命中活 guest RAM");
    } else {
        eprintln!("[peek] VERDICT: 全零/全-FF —— 此刻该 GPA 无可辨内容（换 GPA 或时机重试）");
    }
    Ok(())
}
