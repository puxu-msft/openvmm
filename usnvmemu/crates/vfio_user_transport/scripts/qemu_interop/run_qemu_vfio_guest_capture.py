#!/usr/bin/env python3
# 真 QEMU vfio-user GUEST-BOOT **wire 抓包**变体 —— 为 ADR-013 档2 frozen-vector 取真
# guest-kernel 的 byte-exact wire transcript。
#
# 与 run_qemu_vfio_guest.py 的区别：在 QEMU(真 Linux nvme 驱动)与 nvme_firmware server
# 之间插一个**fd-aware 透明 MITM 代理**，逐帧抓 (方向 / 16B header / payload / fd_count)
# 并原样转发（含 SCM_RIGHTS fd——SET_IRQS 的 eventfd、DMA_MAP 的 memfd——否则 session 断）。
# 抓完把 transcript 落 JSON，供 Rust replay #[test] 冻成 standing CI gate。
#
# **自验**：代理若破坏 fd/分帧，真 guest 必启动失败 / IO 对不上 → 无 GUEST_RESULT=PASS。
# 故"guest 经代理仍 PASS + transcript 非空"= 代理透明正确 + transcript 可信的自证。
#
# ⚠ **已知精度 caveat（freeze 成 replay 黄金前必读）**：header / payload 字节 **100% 准**；
# 但**fd→帧归属在 pipelining 下是近似**——FrameReader 用"组一帧期间收到的 fd 即属该帧"启发，
# 当 client 把 DMA_UNMAP（无 fd）与 DMA_MAP（带 memfd）背靠背一次 sendmsg 合并、单次 recvmsg
# 同时返两帧字节 + 后者的 fd 时，fd 可能被记到前一帧（实测见 cmd 3/DMA_UNMAP 偶现 fd_count=1）。
# **转发不受影响**（同一 fd 集随帧转发，且 server 对"DMA_MAP 无 fd"有 message-mode 降级，故
# guest 仍 PASS）；**但 `fd_count` 字段在 DMA_MAP/UNMAP 边界不可全信**。replay #[test] 冻黄金
# 前须把 fd 归属精确到"与 fd 同 recvmsg 起始的那一帧"（按 absolute byte offset 归属），别直接
# 信本 transcript 的 fd_count。控制面（VERSION/REGION_INFO/IRQ_INFO/SET_IRQS/REGION_RW）不受此
# 影响——SET_IRQS 的 eventfd 与其帧不跨 DMA 边界合并，fd_count 准。
#
# 用法：
#   cd usnvmemu/crates/nvme_firmware && cargo build --features vfio-user
#   eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv bash)"
#   usnvmemu/crates/vfio_user_transport/scripts/qemu_interop/fetch_guest_kernel.sh  # 一次
#   python3 run_qemu_vfio_guest_capture.py [transcript_out.json]
#
# 环境变量：QEMU_BIN / NVME_BIN / GUEST_KERNEL / GUEST_INITRD
import array
import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATES = HERE.parent.parent.parent
DEFAULT_NVME = CRATES / "nvme_firmware" / "target" / "debug" / "nvme_firmware"
CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "usnvmemu_vfio_guest"
MARKER = "VFIO-CAPTURE-" + time.strftime("%Y%m%d%H%M%S")

HEADER_LEN = 16
MAX_FDS = 32  # vfio-user max_msg_fds 上限远低于此；留余量


# ── 复用 run_qemu_vfio_guest.py 的 kernel/initrd 解析（同 CACHE 约定）──
def find_qemu() -> str:
    if env := os.environ.get("QEMU_BIN"):
        return env
    for cand in ("/home/linuxbrew/.linuxbrew/bin/qemu-system-x86_64", "qemu-system-x86_64"):
        if Path(cand).exists() or (cand == "qemu-system-x86_64"):
            return cand
    sys.exit("找不到 qemu-system-x86_64（≥10.1 带 vfio-user-pci）。设 QEMU_BIN。")


def find_kernel() -> Path:
    if env := os.environ.get("GUEST_KERNEL"):
        return Path(env)
    k = CACHE / "vmlinuz"
    if not k.exists():
        sys.exit(f"找不到 guest kernel：{k}（先跑 fetch_guest_kernel.sh）")
    return k


def ensure_initrd() -> Path:
    if env := os.environ.get("GUEST_INITRD"):
        return Path(env)
    ird = CACHE / "initramfs.cpio.gz"
    if not ird.exists():
        sys.exit(f"找不到 initramfs：{ird}（先跑 fetch_guest_kernel.sh / run_qemu_vfio_guest.py 一次以构建）")
    return ird


# ── fd-aware 透明代理 ──────────────────────────────────────────────────
def recvmsg_with_fds(sock: socket.socket, n: int = 65536):
    """一次 recvmsg：返 (data, [fd...])；data 空 = peer 关闭。"""
    fdsize = socket.CMSG_LEN(MAX_FDS * array.array("i").itemsize)
    data, ancdata, _flags, _addr = sock.recvmsg(n, fdsize)
    fds = []
    for level, typ, cdata in ancdata:
        if level == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
            a = array.array("i")
            a.frombytes(cdata[: len(cdata) - (len(cdata) % a.itemsize)])
            fds.extend(a.tolist())
    return data, fds


def sendmsg_with_fds(sock: socket.socket, data: bytes, fds):
    """发 data（处理 partial）+ 把 fds 随**首个** sendmsg 一起送（SCM_RIGHTS）。"""
    if fds:
        anc = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds))]
        sent = sock.sendmsg([data], anc)
    else:
        sent = sock.send(data)
    # 剩余字节（大帧 partial）：fds 已随首发送出，余下纯 send。
    while sent < len(data):
        sent += sock.send(data[sent:])


class FrameReader:
    """从一个 socket 逐帧读 vfio-user 消息（16B header→msg_size→payload），带 fd 归集。"""

    def __init__(self, sock: socket.socket):
        self.sock = sock
        self.buf = bytearray()
        self.fds: list[int] = []

    def _fill(self) -> bool:
        data, fds = recvmsg_with_fds(self.sock)
        if not data:
            return False
        self.buf += data
        self.fds += fds
        return True

    def read_frame(self):
        """返 (frame_bytes, [fd...]) 或 None（peer 关闭）。fds = 组帧期间收到的全部 fd
        （vfio-user 把 fd 随其所属消息一起送，故组一帧期间到的 fd 即属该帧）。"""
        while len(self.buf) < HEADER_LEN:
            if not self._fill():
                return None
        msg_size = struct.unpack_from("<I", self.buf, 4)[0]
        if msg_size < HEADER_LEN:
            raise RuntimeError(f"msg_size {msg_size} < header")
        while len(self.buf) < msg_size:
            if not self._fill():
                return None
        frame = bytes(self.buf[:msg_size])
        del self.buf[:msg_size]
        fds = self.fds
        self.fds = []
        return frame, fds


def _record(transcript, direction: str, frame: bytes, fds):
    msg_id, cmd, msg_size, flags, error_no = struct.unpack_from("<HHIII", frame, 0)
    transcript.append(
        {
            "dir": direction,  # "c2s"(QEMU→server) / "s2c"(server→QEMU)
            "msg_id": msg_id,
            "cmd": cmd,
            "flags": flags,
            "error_no": error_no,
            "header_hex": frame[:HEADER_LEN].hex(),
            "payload_hex": frame[HEADER_LEN:].hex(),
            "fd_count": len(fds),
        }
    )


def _pump(src: socket.socket, dst: socket.socket, direction: str, transcript, lock):
    reader = FrameReader(src)
    try:
        while True:
            got = reader.read_frame()
            if got is None:
                break
            frame, fds = got
            with lock:
                _record(transcript, direction, frame, fds)
            sendmsg_with_fds(dst, frame, fds)
            for fd in fds:
                os.close(fd)  # 转发后关本端 dup（dst 已自持一份）
    except (OSError, RuntimeError):
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def run_proxy(proxy_path: Path, real_path: Path, transcript, lock, ready: threading.Event):
    """listen proxy_path（QEMU 连这里）；每个连接→connect real_path（server）双向泵。"""
    lsock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    lsock.bind(str(proxy_path))
    lsock.listen(1)
    ready.set()
    qemu_sock, _ = lsock.accept()
    srv_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv_sock.connect(str(real_path))
    t1 = threading.Thread(target=_pump, args=(qemu_sock, srv_sock, "c2s", transcript, lock), daemon=True)
    t2 = threading.Thread(target=_pump, args=(srv_sock, qemu_sock, "s2c", transcript, lock), daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    for s in (qemu_sock, srv_sock, lsock):
        try:
            s.close()
        except OSError:
            pass


def main() -> int:
    out_path = Path(sys.argv[1]) if len(sys.argv) > 1 else (HERE / "vfio_guest_wire_transcript.json")
    qemu_bin = find_qemu()
    nvme_bin = Path(os.environ.get("NVME_BIN", DEFAULT_NVME))
    if not nvme_bin.exists():
        sys.exit(f"找不到 nvme_firmware：{nvme_bin}（先 cargo build --features vfio-user）")
    kernel = find_kernel()
    initrd = ensure_initrd()

    tmp = Path(tempfile.mkdtemp(prefix="qemu_vfio_capture_"))
    real_sock = tmp / "nvme_real.sock"
    proxy_sock = tmp / "nvme_proxy.sock"
    img = tmp / "nvme.img"
    srv_log_path = tmp / "server.log"
    serial_path = tmp / "serial.log"
    with open(img, "wb") as f:
        f.truncate(64 * 1024 * 1024)

    transcript: list = []
    lock = threading.Lock()

    srv_log = open(srv_log_path, "w")
    srv = subprocess.Popen(
        [str(nvme_bin), "--vfio-user-sock", str(real_sock), "--backing-file", str(img)],
        env=dict(os.environ, RUST_LOG=os.environ.get("RUST_LOG", "info")),
        stdout=srv_log,
        stderr=subprocess.STDOUT,
    )
    qemu = None
    proxy_thread = None
    try:
        for _ in range(200):
            if real_sock.exists():
                break
            if srv.poll() is not None:
                print(f"=== server 早退 code={srv.returncode} ===")
                print(srv_log_path.read_text()[-2000:])
                return 1
            time.sleep(0.05)

        ready = threading.Event()
        proxy_thread = threading.Thread(
            target=run_proxy, args=(proxy_sock, real_sock, transcript, lock, ready), daemon=True
        )
        proxy_thread.start()
        ready.wait(timeout=5)

        print(f"=== server + 代理 up；引导真 guest（marker={MARKER}）===", flush=True)
        qemu_log_path = tmp / "qemu.log"
        qemu_log = open(qemu_log_path, "w")
        qemu = subprocess.Popen(
            [
                qemu_bin,
                "-machine", "q35,accel=kvm:tcg",
                "-cpu", "host",
                "-smp", "2",
                "-m", "512M",
                "-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                "-machine", "memory-backend=mem",
                "-kernel", str(kernel),
                "-initrd", str(initrd),
                "-append", f"console=ttyS0 panic=-1 rdinit=/init gmarker={MARKER}",
                # **唯一区别**：QEMU 连**代理** socket（代理再转发给 server）。
                "-device", f'{{"driver":"vfio-user-pci","socket":{{"path":"{proxy_sock}","type":"unix"}}}}',
                "-serial", f"file:{serial_path}",
                "-display", "none",
                "-no-reboot",
            ],
            stdout=qemu_log,
            stderr=subprocess.STDOUT,
        )
        try:
            qemu.wait(timeout=120)
        except subprocess.TimeoutExpired:
            print("=== guest 超时（120s）；kill QEMU ===")

        serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
        guest_pass = "GUEST_RESULT=PASS" in serial
        head = img.read_bytes()[:4096]
        host_found = MARKER.encode() in head

        with lock:
            n = len(transcript)
        print(f"=== GUEST_RESULT=PASS: {guest_pass} ; HOST ORACLE: {host_found} ; 抓到 {n} 帧 ===")

        if guest_pass and host_found and n > 0:
            meta = {
                "provenance": {
                    "harness": "run_qemu_vfio_guest_capture.py",
                    "qemu": _qemu_version(qemu_bin),
                    "guest_kernel": _kernel_label(kernel),
                    "captured_marker": MARKER,
                    "note": "byte-exact vfio-user wire；真 Linux nvme 驱动；代理透明转发（含 SCM_RIGHTS fd）已经 guest PASS 自证",
                },
                "frames": transcript,
            }
            out_path.write_text(json.dumps(meta, indent=2, ensure_ascii=False))
            print(f"=== PASS: transcript 已落 {out_path}（{n} 帧）===")
            return 0
        print("=== FAIL：guest 未 PASS 或 transcript 空（代理可能破坏了 fd/分帧）===")
        print("".join(serial.splitlines(keepends=True)[-20:]))
        return 1
    finally:
        if qemu and qemu.poll() is None:
            qemu.terminate()
            try:
                qemu.wait(timeout=3)
            except subprocess.TimeoutExpired:
                qemu.kill()
        if srv.poll() is None:
            srv.terminate()
            try:
                srv.wait(timeout=3)
            except subprocess.TimeoutExpired:
                srv.kill()


def _qemu_version(qemu_bin: str) -> str:
    try:
        out = subprocess.run([qemu_bin, "--version"], capture_output=True, text=True, timeout=10)
        return out.stdout.splitlines()[0] if out.stdout else "unknown"
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def _kernel_label(kernel: Path) -> str:
    # CACHE 里有 linux-image-...deb 名可作版本标
    debs = sorted(CACHE.glob("linux-image-*.deb"))
    return debs[0].name if debs else str(kernel)


if __name__ == "__main__":
    sys.exit(main())
