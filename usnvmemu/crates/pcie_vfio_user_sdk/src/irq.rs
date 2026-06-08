// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! **Phase U5** — SET_IRQS eventfd 收集 + 触发。
//!
//! QEMU 通过 `DEVICE_SET_IRQS` (DATA_EVENTFD + ACTION_TRIGGER) 把 MSI-X
//! 各向量的触发 eventfd 经 SCM_RIGHTS 传给我们；后续 server fire 中断时
//! 往该 eventfd 写 8 字节 u64=1（标准 Linux eventfd 协议）。
//!
//! 我们存储 `Vec<Option<OwnedFd>>`：index = MSI-X 向量号；若 client 用
//! `start=K count=N` 则只覆盖 [K..K+N) 区间；其它 index 保持原值。
//! NONE/BOOL/UNMASK/MASK 等 sub-flag 当前只识别 DATA_EVENTFD+TRIGGER
//! 设/清；其它走 spec-best-effort（log warn + Ok 返回）。

use crate::framing::Message;
use crate::framing::write_message;
use crate::proto::Command;
use crate::proto::Header;
use crate::proto::HeaderFlags;
use crate::proto::IrqSetPayload;
use crate::proto::decode_payload;
use crate::proto::irq_set;
use anyhow::Context as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

/// 单 IRQ 类型（如 MSI-X）的 eventfd 数组。
///
/// 用 `Vec<Option<OwnedFd>>` 而非 `HashMap<u32, OwnedFd>`，访问 O(1)
/// + 顺序保留；index = 向量号 0..N。`None` 表示该向量未配置 trigger fd。
#[derive(Default)]
pub struct IrqVectors {
    /// MSI-X 向量数组。空表示尚未 SET_IRQS。
    vectors: Vec<Option<OwnedFd>>,
}

impl IrqVectors {
    /// 触发向量 `index` 的中断：往 eventfd 写 8 byte u64=1。
    ///
    /// 若 index 越界或 fd=None → 返 false + warn（不 panic）。
    pub fn fire(&mut self, index: u32) -> bool {
        match self.vectors.get_mut(index as usize) {
            Some(Some(fd)) => {
                let one: u64 = 1;
                // eventfd 写 8 byte，永远不会 short-write；任何 IO err 都视
                // 为致命（kernel eventfd 行为）。
                match write_eventfd(fd, &one.to_ne_bytes()) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            index,
                            error = %e,
                            "eventfd write 失败；fd 可能被 QEMU close 或 EAGAIN"
                        );
                        false
                    }
                }
            }
            _ => {
                tracing::warn!(
                    index,
                    len = self.vectors.len(),
                    "fire_interrupt: 向量未配置 eventfd"
                );
                false
            }
        }
    }

    /// 当前注册的向量数（最大 index + 1）。
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }
}

/// 包装 `Write` 让 eventfd 接受 `&[u8]`（OwnedFd 没 impl Write）。
fn write_eventfd(fd: &OwnedFd, buf: &[u8]) -> std::io::Result<()> {
    // SAFETY 替代：用 `BorrowedFd` 取 RawFd + std::fs::File::from(fd.try_clone())
    // 都要 unsafe；直接 nix::unistd::write 是 safe wrapper。
    let _ = nix::unistd::write(fd, buf)?;
    Ok(())
}

/// 处理 DEVICE_SET_IRQS cmd。
///
/// 仅识别 DATA_EVENTFD + ACTION_TRIGGER (typical QEMU MSI-X 设置)；其它
/// flag 组合（MASK/UNMASK/DATA_NONE）当前 best-effort log + 回 OK。
///
/// **review L3** — 顺序：先 payload 长度 → idx 校验，避免短包路径漏校。
/// **review M1** — DATA_EVENTFD + count=0 视作"清 [start..start+0) = 0
/// 个槽位"；本路径默认 no-op，与 spec deassign 区分需 `start` 上下文，
/// 教学版接受 spec 灰区（QEMU 实际只发 count>0 trigger 或 NONE+TRIGGER）。
/// **review M2** — fd 数与 count 不匹配 → EINVAL，避免静默 None 覆盖。
pub fn handle_set_irqs(
    stream: &mut UnixStream,
    vectors: &mut IrqVectors,
    msg_id: u16,
    msg: &mut Message,
) -> anyhow::Result<()> {
    let want = core::mem::size_of::<IrqSetPayload>();
    if msg.payload.len() < want {
        send_err(stream, msg_id, libc::EINVAL as u32)?;
        return Ok(());
    }
    let pl: IrqSetPayload = match decode_payload(&msg.payload[..want]) {
        Ok(p) => p,
        Err(_) => {
            send_err(stream, msg_id, libc::EINVAL as u32)?;
            return Ok(());
        }
    };
    let flags = pl.flags;
    let idx = pl.index;
    let start = pl.start;
    let count = pl.count;
    tracing::debug!(
        flags = format_args!("{flags:#x}"),
        idx,
        start,
        count,
        fd_cnt = msg.fds.len(),
        "DEVICE_SET_IRQS"
    );
    // 我们当前只 wire MSI-X (idx=2)；其它 idx 接受但 no-op。
    if idx != crate::proto::pci_irq::MSIX {
        tracing::debug!(idx, "SET_IRQS for non-MSIX idx: no-op (best-effort OK)");
        let hdr = Header::reply_ok(msg_id, Command::DeviceSetIrqs, 0);
        return write_message(stream, &hdr, &[], &[]).context("write SET_IRQS reply (no-op)");
    }
    // DATA_NONE + ACTION_TRIGGER + count=0 → disable all (清整个数组)。
    let is_data_none = flags & irq_set::DATA_NONE != 0;
    let is_data_eventfd = flags & irq_set::DATA_EVENTFD != 0;
    let is_trigger = flags & irq_set::ACTION_TRIGGER != 0;
    if is_data_none && count == 0 && is_trigger {
        vectors.vectors.clear();
        tracing::info!("SET_IRQS: cleared all MSI-X vectors");
        let hdr = Header::reply_ok(msg_id, Command::DeviceSetIrqs, 0);
        return write_message(stream, &hdr, &[], &[]).context("write SET_IRQS reply (clear)");
    }
    if is_data_eventfd && is_trigger {
        let need = count as usize;
        // **review M2** — fd 数必须 == count。少了静默 None 会让中断丢失。
        if msg.fds.len() != need {
            send_err(stream, msg_id, libc::EINVAL as u32)?;
            return Ok(());
        }
        let upto = (start as usize) + need;
        if vectors.vectors.len() < upto {
            vectors.vectors.resize_with(upto, || None);
        }
        // **review M2** — 从 msg.fds 取走 fd（drain，OwnedFd 移交不被 Message drop close）。
        let mut fd_iter = msg.fds.drain(..);
        for i in 0..need {
            let slot = (start as usize) + i;
            vectors.vectors[slot] = fd_iter.next();
        }
        tracing::info!(
            start,
            count,
            total = vectors.vectors.len(),
            "SET_IRQS: MSI-X trigger eventfds assigned"
        );
        let hdr = Header::reply_ok(msg_id, Command::DeviceSetIrqs, 0);
        return write_message(stream, &hdr, &[], &[]).context("write SET_IRQS reply (assign)");
    }
    // 其它 MASK/UNMASK 等不处理；spec 允许 server 选择不实现。
    tracing::debug!(
        flags = format_args!("{flags:#x}"),
        "SET_IRQS: unsupported flag combo — best-effort OK reply"
    );
    let hdr = Header::reply_ok(msg_id, Command::DeviceSetIrqs, 0);
    write_message(stream, &hdr, &[], &[]).context("write SET_IRQS reply (best-effort)")
}

fn send_err(stream: &mut UnixStream, msg_id: u16, errno: u32) -> anyhow::Result<()> {
    let hdr = Header {
        msg_id,
        cmd: Command::DeviceSetIrqs as u16,
        msg_size: crate::proto::HEADER_LEN as u32,
        flags: HeaderFlags::reply_err().0,
        error_no: errno,
    };
    write_message(stream, &hdr, &[], &[]).context("write SET_IRQS err reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::read_message;
    use crate::framing::write_message as fw_write;
    use crate::proto::pci_irq;
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use zerocopy::IntoBytes;

    fn pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().unwrap()
    }

    #[test]
    fn fire_unconfigured_vector_returns_false() {
        let mut v = IrqVectors::default();
        assert!(!v.fire(0));
        assert!(v.is_empty());
    }

    /// 完整链路：客户端 SET_IRQS 带 2 个 eventfd → server 存表 → fire 写
    /// 8 byte → reader 端读出 counter=1。**review M5** 用真 eventfd 替
    /// `/dev/null`，验证写 8B 原子语义而非 /dev/null 万能 accept。
    #[test]
    fn set_irqs_assign_eventfds_and_fire() {
        use nix::sys::eventfd::EfdFlags;
        use nix::sys::eventfd::EventFd;
        let (mut server, mut client) = pair();
        // 准备 2 个真 eventfd（counter 起始 0）。
        let efd1 = EventFd::from_value_and_flags(0, EfdFlags::empty()).unwrap();
        let efd2 = EventFd::from_value_and_flags(0, EfdFlags::empty()).unwrap();
        let raw1 = efd1.as_raw_fd();
        let raw2 = efd2.as_raw_fd();
        // client 端 dup 一份原 RawFd，让 fire 后还能 read counter 验证
        let efd1_dup = nix::unistd::dup(efd1.as_fd()).unwrap();
        let efd2_dup = nix::unistd::dup(efd2.as_fd()).unwrap();

        let h = thread::spawn(move || -> anyhow::Result<IrqVectors> {
            let mut vectors = IrqVectors::default();
            let mut msg = read_message(&mut server)?;
            handle_set_irqs(&mut server, &mut vectors, msg.header.msg_id, &mut msg)?;
            Ok(vectors)
        });

        let pl = IrqSetPayload {
            argsz: 20,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index: pci_irq::MSIX,
            start: 0,
            count: 2,
        };
        let hdr = Header::command(1, Command::DeviceSetIrqs, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[raw1, raw2]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(!reply.header.flags().is_error());
        let mut vectors = h.join().unwrap().unwrap();
        assert_eq!(vectors.len(), 2);
        // 关掉本地的 EventFd（OwnedFd 在 SCM_RIGHTS 传过去后 server 有 dup）
        drop(efd1);
        drop(efd2);
        assert!(vectors.fire(0));
        assert!(vectors.fire(1));
        assert!(!vectors.fire(2)); // 越界

        // 验证：counter 真的被加到 1（不是 /dev/null 假成功）
        let mut buf = [0u8; 8];
        nix::unistd::read(&efd1_dup, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 1);
        nix::unistd::read(&efd2_dup, &mut buf).unwrap();
        assert_eq!(u64::from_ne_bytes(buf), 1);
    }

    /// DATA_NONE + count=0 + TRIGGER = 清整 vector 数组。
    #[test]
    fn set_irqs_data_none_clears_vectors() {
        use nix::sys::eventfd::EfdFlags;
        use nix::sys::eventfd::EventFd;
        let (mut server, mut client) = pair();
        let mut vectors = IrqVectors::default();
        // 先手 push 一个真 eventfd 占位
        let f = EventFd::from_value_and_flags(0, EfdFlags::empty()).unwrap();
        vectors.vectors.push(Some(f.into()));
        assert_eq!(vectors.len(), 1);

        let h = thread::spawn(move || -> anyhow::Result<IrqVectors> {
            let mut msg = read_message(&mut server)?;
            handle_set_irqs(&mut server, &mut vectors, msg.header.msg_id, &mut msg)?;
            Ok(vectors)
        });
        let pl = IrqSetPayload {
            argsz: 20,
            flags: irq_set::DATA_NONE | irq_set::ACTION_TRIGGER,
            index: pci_irq::MSIX,
            start: 0,
            count: 0,
        };
        let hdr = Header::command(2, Command::DeviceSetIrqs, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let _ = read_message(&mut client).unwrap();
        let vectors = h.join().unwrap().unwrap();
        assert!(vectors.is_empty());
    }

    /// **review M2** — fd 数 < count → EINVAL，不静默 None 覆盖。
    #[test]
    fn set_irqs_fd_count_mismatch_returns_einval() {
        let (mut server, mut client) = pair();
        let mut vectors = IrqVectors::default();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut msg = read_message(&mut server)?;
            handle_set_irqs(&mut server, &mut vectors, msg.header.msg_id, &mut msg)?;
            Ok(())
        });
        let pl = IrqSetPayload {
            argsz: 20,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index: pci_irq::MSIX,
            start: 0,
            count: 3, // 故意要 3 个 fd
        };
        let hdr = Header::command(11, Command::DeviceSetIrqs, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap(); // 只发 0 个 fd
        let reply = read_message(&mut client).unwrap();
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EINVAL as u32);
        h.join().unwrap().unwrap();
    }

    /// SET_IRQS for INTX (idx=0) — no-op + OK reply。
    #[test]
    fn set_irqs_non_msix_is_noop() {
        let (mut server, mut client) = pair();
        let mut vectors = IrqVectors::default();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut msg = read_message(&mut server)?;
            handle_set_irqs(&mut server, &mut vectors, msg.header.msg_id, &mut msg)?;
            Ok(())
        });
        let pl = IrqSetPayload {
            argsz: 20,
            flags: irq_set::DATA_EVENTFD | irq_set::ACTION_TRIGGER,
            index: pci_irq::INTX,
            start: 0,
            count: 0,
        };
        let hdr = Header::command(3, Command::DeviceSetIrqs, pl.as_bytes().len() as u32);
        fw_write(&mut client, &hdr, pl.as_bytes(), &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(!reply.header.flags().is_error());
        h.join().unwrap().unwrap();
    }

    /// payload 长度不对 → EINVAL。
    #[test]
    fn set_irqs_short_payload_rejected() {
        let (mut server, mut client) = pair();
        let mut vectors = IrqVectors::default();
        let h = thread::spawn(move || -> anyhow::Result<()> {
            let mut msg = read_message(&mut server)?;
            handle_set_irqs(&mut server, &mut vectors, msg.header.msg_id, &mut msg)?;
            Ok(())
        });
        let hdr = Header::command(4, Command::DeviceSetIrqs, 4);
        fw_write(&mut client, &hdr, &[0u8; 4], &[]).unwrap();
        let reply = read_message(&mut client).unwrap();
        assert!(reply.header.flags().is_error());
        let err = reply.header.error_no;
        assert_eq!(err, libc::EINVAL as u32);
        h.join().unwrap().unwrap();
    }
}
