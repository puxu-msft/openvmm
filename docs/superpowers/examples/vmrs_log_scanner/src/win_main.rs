// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows-side .vmrs RAM scanner via VmSavedStateDumpProvider.dll.
//!
//! 用 Hyper-V Debug Tools DLL 自动解 XPRESS-HUFF 压缩。
//! 与 `vmrs_log_scanner` 不同，本 binary 必须跑在 Windows 上（依赖 DLL）。
//!
//! 用法：
//!   vmrs_log_scanner_win.exe --path C:\path\to\dump.vmrs \
//!     --needle "[INFO],[WARN],[ERROR],PANIC,panic,pcie_remote,openhcl,underhill"
//!
//! DLL 默认从 Windows SDK 加载：
//!   C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\vmsavedstatedumpprovider.dll
//! 调用方需要：
//!   - 把这个目录加入 PATH，或
//!   - 拷 vmsavedstatedumpprovider.dll 到 .exe 同目录

#![allow(unsafe_code)] // UNSAFETY: FFI to vmsavedstatedumpprovider.dll

#[cfg(not(windows))]
fn main() {
    eprintln!("vmrs_log_scanner_win requires Windows (VmSavedStateDumpProvider.dll)");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

#[cfg(windows)]
mod win {
    use anyhow::Result;
    use anyhow::anyhow;
    use anyhow::bail;
    use clap::Parser;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    mod ffi {
        use std::ffi::c_void;

        #[repr(C)]
        pub struct GpaMemoryChunk {
            pub guest_physical_start_page_index: u64,
            pub page_count: u64,
        }

        #[link(name = "vmsavedstatedumpprovider")]
        unsafe extern "system" {
            pub fn LoadSavedStateFile(
                vmrs_file: *const u16,
                handle: *mut *mut c_void,
            ) -> i32;

            pub fn ReleaseSavedStateFiles(handle: *mut c_void) -> i32;

            pub fn GetGuestPhysicalMemoryChunks(
                handle: *mut c_void,
                page_size: *mut u64,
                chunks: *mut GpaMemoryChunk,
                count: *mut u64,
            ) -> i32;

            pub fn ReadGuestPhysicalAddress(
                handle: *mut c_void,
                physical_address: u64,
                buffer: *mut u8,
                buffer_size: u32,
                bytes_read: *mut u32,
            ) -> i32;
        }
    }

    #[derive(Parser, Debug)]
    #[command(name = "vmrs_log_scanner_win")]
    struct Args {
        /// .vmrs 文件绝对路径
        #[arg(long)]
        path: String,

        /// 最小连续 printable run。
        #[arg(long, default_value_t = 8)]
        min_run: usize,

        /// 用 `,` 分隔的子串，至少命中一个才输出。空 = 全输出。
        #[arg(long, default_value = "")]
        needle: String,

        /// 每次 ReadGuestPhysicalAddress 的 chunk 字节数。
        #[arg(long, default_value_t = 1u32 << 20)]
        read_chunk: u32,
    }

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn check(rc: i32, op: &str) -> Result<()> {
        if rc < 0 {
            Err(anyhow!("{op} failed: HRESULT={:#x}", rc as u32))
        } else {
            Ok(())
        }
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        let needles: Vec<&str> = if args.needle.is_empty() {
            Vec::new()
        } else {
            args.needle.split(',').filter(|s| !s.is_empty()).collect()
        };

        let path_w = to_wide(OsStr::new(&args.path));
        let mut handle: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: path_w 是 NUL-terminated UTF-16; handle 出参合法。
        unsafe {
            check(
                ffi::LoadSavedStateFile(path_w.as_ptr(), &mut handle),
                "LoadSavedStateFile",
            )?
        };

        let result = scan_all(handle, &args, &needles);

        // SAFETY: handle 由 LoadSavedStateFile 返回。
        unsafe { ffi::ReleaseSavedStateFiles(handle) };

        result
    }

    fn scan_all(
        handle: *mut std::ffi::c_void,
        args: &Args,
        needles: &[&str],
    ) -> Result<()> {
        // API 要求 count 输入 = 已分配 buffer 大小（不是 0 探测）。
        // 直接分配 256 个 chunk slot；如果 HRESULT=HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)
        // 再按返回的 count 重试。
        let mut page_size: u64 = 0;
        let mut chunks: Vec<ffi::GpaMemoryChunk> = (0..256)
            .map(|_| ffi::GpaMemoryChunk {
                guest_physical_start_page_index: 0,
                page_count: 0,
            })
            .collect();
        let mut count: u64 = chunks.len() as u64;
        // SAFETY: chunks 长度 == count；out args 合法。
        let rc = unsafe {
            ffi::GetGuestPhysicalMemoryChunks(
                handle,
                &mut page_size,
                chunks.as_mut_ptr(),
                &mut count,
            )
        };
        // ERROR_INSUFFICIENT_BUFFER = 0x7A; HRESULT_FROM_WIN32 = 0x8007007A
        if rc as u32 == 0x8007_007A {
            eprintln!("GetGuestPhysicalMemoryChunks: need {} chunks, retrying", count);
            chunks = (0..count)
                .map(|_| ffi::GpaMemoryChunk {
                    guest_physical_start_page_index: 0,
                    page_count: 0,
                })
                .collect();
            // SAFETY: chunks 长度 == count；out args 合法。
            unsafe {
                check(
                    ffi::GetGuestPhysicalMemoryChunks(
                        handle,
                        &mut page_size,
                        chunks.as_mut_ptr(),
                        &mut count,
                    ),
                    "GetGuestPhysicalMemoryChunks(retry)",
                )?;
            }
        } else if rc < 0 {
            bail!("GetGuestPhysicalMemoryChunks failed: HRESULT={:#x}", rc as u32);
        }
        chunks.truncate(count as usize);
        if chunks.is_empty() {
            bail!("no memory chunks in vmrs");
        }
        eprintln!("vmrs: {} chunk(s), page_size={} bytes", chunks.len(), page_size);

        let mut buf = vec![0u8; args.read_chunk as usize];
        let mut total_hits = 0u64;
        for (i, c) in chunks.iter().enumerate() {
            let start_gpa = c.guest_physical_start_page_index * page_size;
            let end_gpa = start_gpa + c.page_count * page_size;
            eprintln!(
                "chunk {i}: gpa {:#x}..{:#x} ({} pages)",
                start_gpa, end_gpa, c.page_count
            );
            let mut gpa = start_gpa;
            while gpa < end_gpa {
                let want = ((end_gpa - gpa) as usize).min(buf.len()) as u32;
                let mut got: u32 = 0;
                // SAFETY: buf.len() >= want；handle 有效。
                let rc = unsafe {
                    ffi::ReadGuestPhysicalAddress(
                        handle,
                        gpa,
                        buf.as_mut_ptr(),
                        want,
                        &mut got,
                    )
                };
                if rc < 0 {
                    eprintln!(
                        "ReadGuestPhysicalAddress gpa={gpa:#x} HRESULT={:#x}; skip",
                        rc as u32
                    );
                    gpa += want as u64;
                    continue;
                }
                let block = &buf[..got as usize];
                total_hits += scan(gpa, block, args.min_run, needles);
                gpa += got as u64;
                if got == 0 {
                    break;
                }
            }
        }
        eprintln!("emitted {} match(es)", total_hits);
        Ok(())
    }

    fn scan(base_gpa: u64, data: &[u8], min_run: usize, needles: &[&str]) -> u64 {
        let mut hits = 0u64;
        let mut start: Option<usize> = None;
        for (i, &b) in data.iter().enumerate() {
            let p = matches!(b, 0x20..=0x7e | b'\n' | b'\t');
            if p {
                if start.is_none() {
                    start = Some(i);
                }
            } else if let Some(s) = start.take() {
                emit(base_gpa, s, &data[s..i], min_run, needles, &mut hits);
            }
        }
        if let Some(s) = start {
            emit(base_gpa, s, &data[s..], min_run, needles, &mut hits);
        }
        hits
    }

    fn emit(
        base: u64,
        offset: usize,
        run: &[u8],
        min_run: usize,
        needles: &[&str],
        hits: &mut u64,
    ) {
        if run.len() < min_run {
            return;
        }
        let Ok(s) = std::str::from_utf8(run) else {
            return;
        };
        let m = needles.is_empty() || needles.iter().any(|n| s.contains(n));
        if !m {
            return;
        }
        for line in s.lines() {
            let t = line.trim_end();
            if t.is_empty() {
                continue;
            }
            println!("[gpa {:#x}+{:#x}] {t}", base, offset);
            *hits += 1;
        }
    }
}
