// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `.vmrs` (Hyper-V saved state) 中的 RAM 字符串扫描器。
//!
//! 用法（典型 path C 排查）：
//!   ```bash
//!   # 1. Hyper-V 触发 saved state
//!   #   PS> Save-VM pcie-remote-exp
//!   # 2. .vmrs 位于
//!   #   $((Get-VM pcie-remote-exp).Path)\Virtual Machines\<vmid>.vmrs
//!   # 3. 把文件复制到本地，再扫描
//!   vmrs_log_scanner --path dump.vmrs --pattern '\[INFO\]|\[WARN\]|\[ERROR\]|PANIC|panic'
//!   ```
//!
//! 实现：
//! - 用 hvs_file::reader::HvsFileReader 打开 .vmrs
//! - 遍历 `/savedstate/RamBlock{n}` 数据块（每块 1 MiB，原始 GPA 内容）
//! - 在每块中找：
//!   * ASCII / UTF-8 子串（保留至少 N 字节连续可打印）
//!   * 含 regex pattern 的行
//! - 输出 `[block N @ offset OFF] <text>`
//!
//! 注意：
//! - 不重排顺序；不去重。boot_logger StringBuffer 是 ring buffer 风格的
//!   append-only 文本，所以子串扫描即可命中。
//! - VTL2 RAM 在 .vmrs 中**和 VTL0 RAM 是同一块**（OpenHCL paravisor 模式下，
//!   VTL2 只是 host 划出的低位 PFN）。

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use hvs_file::reader::HvsFileReader;
use std::fs::File;

#[derive(Parser, Debug)]
#[command(
    name = "vmrs_log_scanner",
    about = "Scan Hyper-V .vmrs RAM blocks for printable strings (OpenHCL boot_logger diagnosis)"
)]
struct Args {
    /// .vmrs file path
    #[arg(long)]
    path: String,

    /// Minimum length of a printable run to emit.
    #[arg(long, default_value_t = 8)]
    min_run: usize,

    /// Only emit strings containing one of these substrings (case-sensitive).
    /// 用 `,` 分隔多个。 例：`--needle '[INFO],[WARN],PANIC,pcie_remote'`
    #[arg(long, default_value = "")]
    needle: String,

    /// 最多扫多少个 1 MiB 数据块；0 表示全部。
    #[arg(long, default_value_t = 0)]
    max_blocks: usize,

    /// 输出每块的字节统计 header。
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let needles: Vec<&str> = if args.needle.is_empty() {
        Vec::new()
    } else {
        args.needle.split(',').filter(|s| !s.is_empty()).collect()
    };

    let f = File::open(&args.path).with_context(|| format!("open {}", args.path))?;
    let mut reader = HvsFileReader::open(f).context("HvsFileReader::open")?;

    // RamBlockN 是流式：连续编号，不一定从 0 开始（但 vmrs_writer 总是从 0）
    let mut total_hits = 0u64;
    let mut block_idx = 0u64;
    loop {
        let key = format!("/savedstate/RamBlock{block_idx}");
        if !reader.contains_key(&key) {
            // 第一个 miss 后就停（数据块编号连续）
            if block_idx == 0 {
                anyhow::bail!("no /savedstate/RamBlock0 in {}", args.path);
            }
            break;
        }
        let data = reader
            .read_array(&key)
            .with_context(|| format!("read {key}"))?;
        if args.verbose {
            eprintln!("block {block_idx}: {} bytes", data.len());
        }
        let block_hits = scan_block(block_idx, &data, args.min_run, &needles);
        total_hits += block_hits;
        block_idx += 1;
        if args.max_blocks != 0 && block_idx >= args.max_blocks as u64 {
            break;
        }
    }
    eprintln!(
        "scanned {} block(s), {} match(es) emitted",
        block_idx, total_hits
    );
    Ok(())
}

/// 扫一个 1 MiB block，按 ASCII printable run 提取并按 needle filter 输出。
///
/// printable = `0x20..=0x7e` 或 `\n` 或 `\t`（保留换行让多行 log 不被打散）。
fn scan_block(idx: u64, data: &[u8], min_run: usize, needles: &[&str]) -> u64 {
    let mut hits = 0u64;
    let mut start: Option<usize> = None;
    for (i, &b) in data.iter().enumerate() {
        let printable = matches!(b, 0x20..=0x7e | b'\n' | b'\t');
        if printable {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            let run = &data[s..i];
            if run.len() >= min_run {
                emit_if_match(idx, s, run, needles, &mut hits);
            }
        }
    }
    if let Some(s) = start {
        let run = &data[s..];
        if run.len() >= min_run {
            emit_if_match(idx, s, run, needles, &mut hits);
        }
    }
    hits
}

fn emit_if_match(idx: u64, offset: usize, run: &[u8], needles: &[&str], hits: &mut u64) {
    let Ok(s) = std::str::from_utf8(run) else {
        return;
    };
    let matched = needles.is_empty() || needles.iter().any(|n| s.contains(n));
    if matched {
        // 按行分行打印，便于和 boot_logger 输出对齐。
        for line in s.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            println!("[block {idx} @ {offset:#x}] {trimmed}");
            *hits += 1;
        }
    }
}
