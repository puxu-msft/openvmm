// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// 示例代码：放宽一些 clippy lint，便于按 NVMe spec 字段名照搬写代码。
#![allow(clippy::unnecessary_cast)] // u32 spec 字段保留 `as u32` 增强可读性
#![allow(clippy::too_many_arguments)] // NVMe SQE/CQE 字段多，wrap struct 反而绕
#![allow(clippy::enum_variant_names)] // PendingOp 全 Nvm 前缀强调 NVMe 语义
#![allow(unused_imports)] // cmd.rs 把 IntoBytes 既用于 derive 又用于函数体；该 lint 误判

//! Userspace NVMe controller — library 入口。
//!
//! **Phase V0** — 拆成 `lib + bin`：
//! - `lib.rs`（本文件）re-export `cmd / controller / pi / regs / sgl` 五大模块，
//!   让其它 crate（如即将到来的 `nvme_of_tcp_target`）可 `use nvme_firmware::controller::NvmeController` 复用 controller body。
//! - `main.rs` 不动语义，只把 `mod cmd;` 等 module 声明换 `pub use lib::*`
//!   重导出，原 binary 入口 + CLI / runtime / serve_unix 都保留。
//!
//! 这样 `NvmeController + cmd::* + sgl::* + pi::*` 不再被 binary 私藏。
//! Phase V (NVMe-oF TCP) 将作为兄弟 crate 直接 `use` 这些类型，不复制代码。

pub mod cmd;
pub mod controller;
pub mod pi;
pub mod regs;
pub mod sgl;

pub use controller::NvmeController;
pub use controller::{DEFAULT_MAX_QUEUE_ENTRIES, MAX_MAX_QUEUE_ENTRIES, MIN_MAX_QUEUE_ENTRIES};
