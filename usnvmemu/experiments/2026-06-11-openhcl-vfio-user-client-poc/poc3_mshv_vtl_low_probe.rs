// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// POC-3 探针（真 OpenHCL VTL2 内跑）：验证一个 **非-underhill** 用户态进程能否
// open `/dev/mshv_vtl_low` 并 mmap VTL0 guest RAM（线性 GPA→file_offset），
// 做真读写——即 Spec A「firmware 直访 guest 内存零拷贝」的 OpenHCL 侧承重假设。
//
// 机制已被生产代码证明：`openhcl/underhill_mem/src/mapping.rs` 即 open 此设备 +
// `map_file(base, len, fd, file_offset, true)` 线性映射 VTL0 RAM。本探针确认
// **另一个进程**也能做同样的事（设备不限定只给 underhill）。
//
// 构建（静态 musl，便于塞进 VTL2 initrd / 经 ohcldiag-dev run 执行）：
//   rustc --edition 2021 -O --target x86_64-unknown-linux-musl \
//       poc3_mshv_vtl_low_probe.rs -o poc3_probe
//   （或 cargo + target musl；本文件零外部 crate，纯 libc syscalls via std）
//
// 在真 VTL2 跑（GPA/LEN 由调用方按 guest memory_layout 给；CVM 下 file_offset 可能
// 需带 SHARED_MEMORY_FLAG=1<<63 取 shared 视图——非-CVM 用裸 GPA）：
//   POC_GPA=0x100000 POC_LEN=0x1000 ./poc3_probe
//
// 退出码 0 = open+mmap+读写 guest RAM 成功（非-underhill 进程直访可行）。

use std::os::fd::AsRawFd;

const DEV: &str = "/dev/mshv_vtl_low";

fn env_u64(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Ok(s) => {
            let s = s.trim();
            if let Some(hex) = s.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).unwrap_or(default)
            } else {
                s.parse().unwrap_or(default)
            }
        }
        Err(_) => default,
    }
}

fn main() {
    let gpa = env_u64("POC_GPA", 0x10_0000); // 默认 1 MiB 处（避开低地址特殊页）
    let len = env_u64("POC_LEN", 0x1000) as usize;
    let shared_flag = env_u64("POC_SHARED_FLAG", 0); // CVM: 设 0x8000000000000000 取 shared 视图

    eprintln!("[poc3] open {DEV} ...");
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(DEV) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[poc3] FAIL: open {DEV}: {e}（非 OpenHCL VTL2 环境？或权限不足）");
            std::process::exit(1);
        }
    };
    let file_offset = gpa | shared_flag;
    eprintln!("[poc3] mmap len={len:#x} file_offset={file_offset:#x} (GPA={gpa:#x}) ...");

    // SAFETY: mmap 一段 len 字节的 MAP_SHARED 读写映射；fd 来自上面成功 open 的设备，
    // file_offset 按 OpenHCL 线性映射约定 = GPA（| shared flag）。失败返回 MAP_FAILED 已检查。
    let addr = unsafe {
        libc_mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            file.as_raw_fd(),
            file_offset as i64,
        )
    };
    if addr == MAP_FAILED {
        let e = std::io::Error::last_os_error();
        eprintln!("[poc3] FAIL: mmap: {e}（设备可能要求不同 file_offset 语义 / 该 GPA 不可映射）");
        std::process::exit(1);
    }

    // SAFETY: addr 为 mmap 成功返回的 len 字节有效映射，按 u8 读写不越界。
    let slice = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, len) };
    let before = u64::from_le_bytes(slice[0..8].try_into().unwrap());
    eprintln!("[poc3] 读 GPA[{gpa:#x}] 首 8 字节 = {before:#x}");

    let marker: u64 = 0x5A5A_0000_0000_5A5A;
    slice[0..8].copy_from_slice(&marker.to_le_bytes());
    let after = u64::from_le_bytes(slice[0..8].try_into().unwrap());

    // SAFETY: 解除上面的映射。
    unsafe { libc_munmap(addr, len) };

    if after == marker {
        eprintln!("[poc3] 写回 marker + 重读一致 ⟹ 非-underhill VTL2 进程 mmap 直访 guest RAM 可行。");
        eprintln!("[poc3] POC-3 PASSED ✓");
        std::process::exit(0);
    }
    eprintln!("[poc3] FAIL: 写回不一致 (got {after:#x})");
    std::process::exit(1);
}

// ── 极小 libc FFI（避免拉 crate 依赖，便于纯 rustc 静态构建）──────────────
const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const MAP_SHARED: i32 = 0x1;
const MAP_FAILED: *mut core::ffi::c_void = usize::MAX as *mut core::ffi::c_void;

unsafe extern "C" {
    #[link_name = "mmap"]
    fn libc_mmap(
        addr: *mut core::ffi::c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut core::ffi::c_void;
    #[link_name = "munmap"]
    fn libc_munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
}
