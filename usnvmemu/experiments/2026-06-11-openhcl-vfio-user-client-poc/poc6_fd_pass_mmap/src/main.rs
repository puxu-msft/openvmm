// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// POC-6：验证 audit 命中的核心 seam ——
//   "一个进程 open /dev/mshv_vtl_low，经 SCM_RIGHTS 把该 fd 传给**另一个进程**，
//    后者（从未自开该设备）能否按 file_offset=GPA mmap 真 guest RAM 并读写。"
//
// 这正是 Spec A 数据路径默认成立、却没被 POC-1（传自造 memfd）/ POC-3（自开设备）
// 任一验过的衔接点。在真 OpenHCL VTL2 内跑（经 ohcldiag-dev stdin+base64 投递）。
//
// 角色：parent = "underhill"（持设备 fd 的特权方），child = "firmware"（只从
// SCM_RIGHTS 拿 fd 的受信方）。child 成功 mmap+读写 ⟹ seam 成立。
//
// 退出码 0 = PASS（child 经传入 fd 访问真 guest RAM 成功）。

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};
use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg, socketpair, AddressFamily,
    SockFlag, SockType,
};
use nix::unistd::{ForkResult, fork};
use std::io::{IoSlice, IoSliceMut};
use std::num::NonZeroUsize;

const DEV: &str = "/dev/mshv_vtl_low";

fn env_u64(k: &str, d: u64) -> u64 {
    let Ok(s) = std::env::var(k) else { return d };
    let s = s.trim();
    let parsed = match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse::<u64>(),
    };
    parsed.unwrap_or(d)
}

fn child(recv_fd: RawFd, gpa: u64, len: usize, shared_flag: u64) -> i32 {
    // 从 socket 收一个 fd（SCM_RIGHTS）。child 自己**没有** open 过 mshv_vtl_low。
    let mut buf = [0u8; 8];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let mut cmsg_space = nix::cmsg_space!(RawFd);
    let msg = match recvmsg::<()>(recv_fd, &mut iov, Some(&mut cmsg_space), MsgFlags::empty()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[poc6/child] recvmsg fail: {e}");
            return 1;
        }
    };
    let mut dev_fd: Option<OwnedFd> = None;
    for c in msg.cmsgs().unwrap() {
        if let ControlMessageOwned::ScmRights(fds) = c {
            if let Some(&raw) = fds.first() {
                // SAFETY: raw 是内核经 SCM_RIGHTS 新装入本进程的 fd，独占所有权。
                dev_fd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
    }
    let Some(dev_fd) = dev_fd else {
        eprintln!("[poc6/child] 未从 SCM_RIGHTS 收到 fd");
        return 1;
    };
    eprintln!("[poc6/child] 收到 mshv_vtl_low fd（非自开）；mmap file_offset={:#x} (GPA={gpa:#x}) ...",
        gpa | shared_flag);

    let nz = NonZeroUsize::new(len).unwrap();
    // SAFETY: 对收到的设备 fd 做 len 字节 MAP_SHARED 读写映射，file_offset = GPA(|flag)。
    let addr = match unsafe {
        mmap(None, nz, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
             MapFlags::MAP_SHARED, &dev_fd, (gpa | shared_flag) as i64)
    } {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[poc6/child] mmap fail: {e}（传入 fd 不可 mmap？这会证伪 seam）");
            return 1;
        }
    };
    // SAFETY: addr 为 mmap 成功返回的 len 字节有效映射。
    let slice = unsafe { std::slice::from_raw_parts_mut(addr.as_ptr() as *mut u8, len) };
    let before = u64::from_le_bytes(slice[0..8].try_into().unwrap());
    eprintln!("[poc6/child] 读 GPA[{gpa:#x}] 首 8 字节 = {before:#x}（经传入 fd）");
    let marker: u64 = 0x9E66_0000_0000_9E66;
    slice[0..8].copy_from_slice(&marker.to_le_bytes());
    let after = u64::from_le_bytes(slice[0..8].try_into().unwrap());
    // SAFETY: 解除映射。
    let _ = unsafe { munmap(addr, len) };
    if after == marker {
        eprintln!("[poc6/child] 写回 marker + 重读一致 ⟹ 经 SCM_RIGHTS 传入的 fd 可 mmap 真 guest RAM。");
        0
    } else {
        eprintln!("[poc6/child] 写回不一致 got={after:#x}");
        1
    }
}

fn main() {
    let gpa = env_u64("POC_GPA", 0x10_0000);
    let len = env_u64("POC_LEN", 0x1000) as usize;
    let shared_flag = env_u64("POC_SHARED_FLAG", 0);

    let (a, b) = match socketpair(AddressFamily::Unix, SockType::Stream, None, SockFlag::empty()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[poc6] socketpair fail: {e}");
            std::process::exit(1);
        }
    };

    // SAFETY: fork。两侧各自只用自己那半 socket + 走不相交路径。
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            drop(a);
            std::process::exit(child(b.as_raw_fd(), gpa, len, shared_flag));
        }
        Ok(ForkResult::Parent { child: pid }) => {
            drop(b);
            // parent = "underhill"：open 设备并把 fd 经 SCM_RIGHTS 传给 child。
            let dev = match std::fs::OpenOptions::new().read(true).write(true).open(DEV) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[poc6/parent] open {DEV} fail: {e}（非 VTL2 环境？）");
                    std::process::exit(1);
                }
            };
            eprintln!("[poc6/parent] open {DEV} OK；经 SCM_RIGHTS 传 fd 给 child ...");
            let fds = [dev.as_raw_fd()];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            let iov = [IoSlice::new(b"POC6FDPS")];
            if let Err(e) = sendmsg::<()>(a.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None) {
                eprintln!("[poc6/parent] sendmsg fail: {e}");
                std::process::exit(1);
            }
            // 等 child 完成
            match nix::sys::wait::waitpid(pid, None) {
                Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                    if code == 0 {
                        eprintln!("[poc6] PASS ✓ — child 经 SCM_RIGHTS 传入的 mshv_vtl_low fd 成功 mmap 真 guest RAM");
                        eprintln!("[poc6] ⟹ Spec A 的'underhill 传 fd 给 firmware mmap'数据路径 seam 成立。");
                    } else {
                        eprintln!("[poc6] FAIL — child 退出码 {code}");
                    }
                    std::process::exit(code);
                }
                other => {
                    eprintln!("[poc6] waitpid 异常: {other:?}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("[poc6] fork fail: {e}");
            std::process::exit(1);
        }
    }
}
