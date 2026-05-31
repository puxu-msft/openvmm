// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Reservation 命令处理 + Reservation Status Data Structure 序列化。
//!
//! NVMe spec § 6.11 (Acquire) / § 6.13 (Register) / § 6.14 (Report) /
//! § 6.15 (Release)。
//!
//! Phase J 模块化重构：原 `apply_reservation_cmd` 在 mod.rs，
//! `build_reservation_report` 在 io.rs，集中到此处。
//!
//! ## 设计简化
//!
//! 单 controller 模型下 host 由 8-byte reservation key 唯一标识；真硬件
//! 用 16-byte HOSTID + rkey 组合（spec § 6.13 Connect cmd）。Phase K9
//! 计划重构成真 multi-host HOSTID。
//!
//! ## reservation type 编码（spec § 8.19.1）
//!
//! - 1 = Write Exclusive
//! - 2 = Exclusive Access
//! - 3 = Write Exclusive Registrants Only
//! - 4 = Exclusive Access Registrants Only
//! - 5 = Write Exclusive All Registrants
//! - 6 = Exclusive Access All Registrants
//!
//! ## reservation_gen 不变量（reviewer M3 修复）
//!
//! Reservation Report 的 GEN 字段必须**单调递增**（即使 unregister）。
//! 所有 mutate 路径调 `Namespace::bump_gen()` 保证。

use super::Namespace;
use super::NvmeController;
use super::ReservationKind;
use crate::cmd::*;
use pcie_remote_userspace_sdk::*;

impl Namespace {
    /// 单调递增 reservation_gen（spec § 6.14 driver 用此感知 state 变化）。
    fn bump_gen(&mut self) {
        self.reservation_gen = self.reservation_gen.wrapping_add(1);
    }
    /// **K9** — 按 rkey 找 registrant index。
    fn rkey_pos(&self, k: u64) -> Option<usize> {
        self.registrants.iter().position(|&(rk, _, _)| rk == k)
    }
    /// **K9** — 按 rkey 检查是否已注册。
    fn has_rkey(&self, k: u64) -> bool {
        self.rkey_pos(k).is_some()
    }
}

impl NvmeController {
    /// 处理 Reservation Register/Acquire/Release 数据（DMA-read 完成后调）。
    ///
    /// 数据格式（spec § 6.13/6.11/6.15）：
    /// - Register: 16 byte = CRKEY (8) + NRKEY (8)
    /// - Acquire:  16 byte = CRKEY (8) + PRKEY (8)
    /// - Release:   8 byte = CRKEY (8)
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_reservation_cmd(
        &mut self,
        nsid: u32,
        kind: ReservationKind,
        action: u8,
        rtype: u8,
        data: &[u8],
        cid: u16,
        sq_id: u16,
        sq_head: u16,
        phase: u8,
    ) -> Cqe {
        // K9：取 controller-level host_id 快照（在借 ns 前）。
        let c_host_id_lo = self.host_id_lo;
        let c_host_id_hi = self.host_id_hi;
        let Some(ns) = self.namespaces.get_mut(&nsid) else {
            return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_NAMESPACE, 0);
        };
        let read_u64 = |buf: &[u8], off: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&buf[off..off + 8]);
            u64::from_le_bytes(a)
        };
        match kind {
            ReservationKind::Register => {
                if data.len() < 16 {
                    return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
                }
                let crkey = read_u64(data, 0);
                let nrkey = read_u64(data, 8);
                match action {
                    0 => {
                        // Register: 注册 nrkey 为本 host 的 key。要求 host 之前
                        // 未注册（避免重复）。
                        if ns.has_rkey(nrkey) {
                            tracing::warn!(
                                nsid,
                                nrkey,
                                "Reservation Register: key already registered"
                            );
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        // K9：把 controller 当前 host_id 一起记，若 driver
                        // 未 Set Features 0x81 设过则 (0, 0)，等价 anonymous。
                        ns.registrants.push((nrkey, c_host_id_lo, c_host_id_hi));
                        ns.bump_gen();
                        tracing::info!(nsid, nrkey, "Reservation Register OK");
                    }
                    1 => {
                        // Unregister: 移除 crkey（若 crkey 还持有 reservation
                        // 也一并清）。
                        let was_present = ns.has_rkey(crkey);
                        ns.registrants.retain(|&(rk, _, _)| rk != crkey);
                        if let Some((holder, _)) = ns.reservation
                            && holder == crkey
                        {
                            ns.reservation = None;
                        }
                        if was_present {
                            ns.bump_gen();
                        }
                        tracing::info!(nsid, crkey, "Reservation Unregister OK");
                    }
                    2 => {
                        // Replace: 把 crkey 替换为 nrkey
                        // **reviewer H1 修复**：nrkey 不能与现有 registrant
                        // 冲突，否则 Vec 中出现重复 → Register 后续 contains
                        // check 失真。spec § 6.13: "If the New Reservation Key
                        // equals an existing key, the action shall fail."
                        if ns.has_rkey(nrkey) && nrkey != crkey {
                            tracing::warn!(
                                nsid,
                                nrkey,
                                "Reservation Replace: nrkey conflicts existing registrant"
                            );
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        if let Some(pos) = ns.rkey_pos(crkey) {
                            let (_, hid_lo, hid_hi) = ns.registrants[pos];
                            ns.registrants[pos] = (nrkey, hid_lo, hid_hi);
                            if let Some((holder, t)) = ns.reservation
                                && holder == crkey
                            {
                                ns.reservation = Some((nrkey, t));
                            }
                            ns.bump_gen();
                        } else {
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        tracing::info!(nsid, crkey, nrkey, "Reservation Replace OK");
                    }
                    _ => return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0),
                }
            }
            ReservationKind::Acquire => {
                if data.len() < 16 {
                    return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
                }
                let crkey = read_u64(data, 0);
                let prkey = read_u64(data, 8);
                if !ns.has_rkey(crkey) {
                    tracing::warn!(nsid, crkey, "Acquire: crkey not registered");
                    return Cqe::error(cid, sq_id, sq_head, phase, sc::RESERVATION_CONFLICT, 0);
                }
                if !(1..=6).contains(&rtype) {
                    return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
                }
                match action {
                    0 => {
                        // Acquire: 仅当无 reservation 时成功
                        if ns.reservation.is_some() {
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        ns.reservation = Some((crkey, rtype));
                        ns.bump_gen();
                        tracing::info!(nsid, crkey, rtype, "Reservation Acquire OK");
                    }
                    1 | 2 => {
                        // Preempt (+ optional Abort)。spec 复杂；简化：若
                        // 当前 holder == prkey 则替换，否则失败。
                        if let Some((holder, _)) = ns.reservation
                            && holder != prkey
                        {
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        ns.reservation = Some((crkey, rtype));
                        ns.bump_gen();
                        tracing::info!(nsid, crkey, prkey, rtype, "Reservation Preempt OK");
                    }
                    _ => return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0),
                }
            }
            ReservationKind::Release => {
                if data.len() < 8 {
                    return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0);
                }
                let crkey = read_u64(data, 0);
                match action {
                    0 => {
                        // Release：仅当本 host 持有时清。
                        // **reviewer H2 修复**：区分 holder 不匹配（CONFLICT）
                        // vs rtype 不匹配（INVALID_FIELD）。spec § 6.15。
                        match ns.reservation {
                            Some((holder, t)) if holder == crkey && t == rtype => {
                                ns.reservation = None;
                                ns.bump_gen();
                                tracing::info!(nsid, crkey, "Reservation Release OK");
                            }
                            Some((holder, t)) if holder == crkey && t != rtype => {
                                tracing::warn!(
                                    nsid,
                                    crkey,
                                    holder_type = t,
                                    requested = rtype,
                                    "Release rtype mismatch (INVALID_FIELD)"
                                );
                                return Cqe::error(
                                    cid,
                                    sq_id,
                                    sq_head,
                                    phase,
                                    sc::INVALID_FIELD,
                                    0,
                                );
                            }
                            _ => {
                                return Cqe::error(
                                    cid,
                                    sq_id,
                                    sq_head,
                                    phase,
                                    sc::RESERVATION_CONFLICT,
                                    0,
                                );
                            }
                        }
                    }
                    1 => {
                        // Clear：全 NS 所有 reservation 清空（spec 通常仅
                        // 当前 holder 可调）
                        if let Some((holder, _)) = ns.reservation
                            && holder != crkey
                        {
                            return Cqe::error(
                                cid,
                                sq_id,
                                sq_head,
                                phase,
                                sc::RESERVATION_CONFLICT,
                                0,
                            );
                        }
                        if ns.reservation.is_some() {
                            ns.reservation = None;
                            ns.bump_gen();
                        }
                        tracing::info!(nsid, "Reservation Clear OK");
                    }
                    _ => return Cqe::error(cid, sq_id, sq_head, phase, sc::INVALID_FIELD, 0),
                }
            }
        }
        Cqe::success(cid, sq_id, sq_head, phase)
    }
}

/// 构造 Reservation Status Data Structure (spec § 6.14 Figure 197)。
///
/// 64-byte header + 24-byte * 每 registrant。
pub(super) fn build_reservation_report(ns: &Namespace, bytes: usize) -> Vec<u8> {
    let n_reg = ns.registrants.len() as u16;
    let total = 64 + (n_reg as usize) * 24;
    let mut buf = vec![0u8; bytes.max(total)];
    // GEN @ 0..4 — reviewer M3 修复：用单调 reservation_gen 而非 n_reg
    buf[0..4].copy_from_slice(&ns.reservation_gen.to_le_bytes());
    // RTYPE @ 4
    buf[4] = ns.reservation.map(|(_, t)| t).unwrap_or(0);
    // REGCTL @ 5..7
    buf[5..7].copy_from_slice(&n_reg.to_le_bytes());
    // 每 registrant @ 64 + i*24
    for (i, &(rkey, hid_lo, _hid_hi)) in ns.registrants.iter().enumerate() {
        let off = 64 + i * 24;
        if off + 24 > buf.len() {
            break;
        }
        // CNTLID (2) — 单 controller 用 1
        buf[off..off + 2].copy_from_slice(&1u16.to_le_bytes());
        // RCSTS (1) bit 0 = holds reservation
        let holds = ns
            .reservation
            .is_some_and(|(holder_key, _)| holder_key == rkey);
        buf[off + 2] = if holds { 0x01 } else { 0x00 };
        // HOSTID (8) @ off+8 — K9：真 HOSTID lo 64-bit；未设置时用 rkey 复用
        let hostid = if hid_lo != 0 { hid_lo } else { rkey };
        buf[off + 8..off + 16].copy_from_slice(&hostid.to_le_bytes());
        // RKEY (8) @ off+16
        buf[off + 16..off + 24].copy_from_slice(&rkey.to_le_bytes());
    }
    buf.truncate(bytes);
    buf
}
