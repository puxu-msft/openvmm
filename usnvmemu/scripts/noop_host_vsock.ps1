# PowerShell vsock client for pcie_remote experimental device.
#
# 角色：vsock client。OpenHCL（VTL2 内）是 server (vsock listen on $VsockPort)。
# 此脚本在 Windows host 上跑 AF_HYPERV connect 到 (target_vm_id, service_guid)。
#
# 协议：与 docs/superpowers/examples/pcie_remote_test_harness/src/main.rs 完全等价
# 的最小 noop 设备，但走 vsock 而非 TCP。
#
# 用法（管理员或 Hyper-V Administrators）：
#   .\noop_host_vsock.ps1 -VmName MyExpVM -VsockPort 50000
#
# 依赖：Windows 10 1809+（AF_HYPERV 自带）+ PowerShell 5.1 / 7+
# 协议字段定义请对照 vm/devices/pcie_remote_protocol/proto/pcie_remote.proto。
#
# ⚠ 这是最小可工作版本，仅响应 Hello + MMIO read 0。完整的 host SDK
# （DMA、interrupt、cfg-side-effect）请用 Rust 版 host_stub（在 Windows 上
# 跨编或原生编 docs/superpowers/examples/pcie_remote_test_harness）。

param(
    [Parameter(Mandatory=$true)]
    [string]$VmName,
    [Parameter(Mandatory=$true)]
    [uint32]$VsockPort
)

$ErrorActionPreference = 'Stop'

# ----- 1. 从 VM 名拿 VmId -----
$vm = Get-VM -Name $VmName -ErrorAction Stop
$VmId = $vm.Id
Write-Host "Connecting to VM '$VmName' (Id=$VmId) on vsock port $VsockPort"

# ----- 2. 计算 service GUID（与 setup-pcie-remote.ps1 一致）-----
# HV_GUID_VSOCK_TEMPLATE = 00000000-facb-11e6-bd58-64006a7986d3
# service GUID = <port hex>-facb-11e6-bd58-64006a7986d3
$ServiceGuid = ('{0:x8}' -f $VsockPort) + '-facb-11e6-bd58-64006a7986d3'
Write-Host "Service GUID = $ServiceGuid"

# ----- 3. AF_HYPERV socket 连接 -----
# 这里需要用 P/Invoke 调 Win32 Winsock，因为 .NET Sockets 不直接支持 AF_HYPERV。
# 简化：直接用 sockaddr_hv + WSAConnect。
Add-Type -Namespace HvSock -Name Native -MemberDefinition @'
using System;
using System.Runtime.InteropServices;

public class Win {
    public const int AF_HYPERV = 34;
    public const int SOCK_STREAM = 1;
    public const int HV_PROTOCOL_RAW = 1;

    [StructLayout(LayoutKind.Sequential)]
    public struct SOCKADDR_HV {
        public ushort Family;
        public ushort Reserved;
        public Guid VmId;
        public Guid ServiceId;
    }

    [DllImport("ws2_32.dll", SetLastError=true)]
    public static extern int WSAStartup(ushort wVersionRequested, out byte[] lpWSAData);

    [DllImport("ws2_32.dll", SetLastError=true, EntryPoint="WSASocketW")]
    public static extern IntPtr WSASocket(int af, int type, int protocol,
        IntPtr lpProtocolInfo, int g, int dwFlags);

    [DllImport("ws2_32.dll", SetLastError=true)]
    public static extern int connect(IntPtr s, ref SOCKADDR_HV name, int namelen);

    [DllImport("ws2_32.dll", SetLastError=true)]
    public static extern int send(IntPtr s, byte[] buf, int len, int flags);

    [DllImport("ws2_32.dll", SetLastError=true)]
    public static extern int recv(IntPtr s, byte[] buf, int len, int flags);

    [DllImport("ws2_32.dll")]
    public static extern int closesocket(IntPtr s);

    [DllImport("ws2_32.dll")]
    public static extern int WSAGetLastError();
}
'@ -Language CSharp -ErrorAction Stop

# WSAStartup（PowerShell 进程可能没自动 init）
$wsaData = New-Object byte[] 408
[HvSock.Native.Win]::WSAStartup(0x0202, [ref]$wsaData) | Out-Null

$sock = [HvSock.Native.Win]::WSASocket(
    [HvSock.Native.Win]::AF_HYPERV,
    [HvSock.Native.Win]::SOCK_STREAM,
    [HvSock.Native.Win]::HV_PROTOCOL_RAW,
    [IntPtr]::Zero, 0, 0)
if ($sock -eq [IntPtr](-1)) {
    $err = [HvSock.Native.Win]::WSAGetLastError()
    throw "WSASocket failed: $err"
}

$addr = New-Object HvSock.Native.Win+SOCKADDR_HV
$addr.Family = [HvSock.Native.Win]::AF_HYPERV
$addr.Reserved = 0
$addr.VmId = [Guid]$VmId
$addr.ServiceId = [Guid]$ServiceGuid

$size = [System.Runtime.InteropServices.Marshal]::SizeOf($addr)
Write-Host "Connecting (sockaddr size = $size)..."

# 重试：OpenHCL 启动期 vsock listener 可能未就绪
$ok = $false
for ($i = 0; $i -lt 30; $i++) {
    $r = [HvSock.Native.Win]::connect($sock, [ref]$addr, $size)
    if ($r -eq 0) { $ok = $true; break }
    $err = [HvSock.Native.Win]::WSAGetLastError()
    Write-Host "connect attempt $i failed: WSA $err, retry in 1s"
    Start-Sleep -Seconds 1
}
if (-not $ok) {
    [HvSock.Native.Win]::closesocket($sock) | Out-Null
    throw "connect failed after 30 retries"
}

Write-Host "Connected. Reading Hello frame..."

# ----- 4. 协议帧 codec（与 codec.rs 等价）-----
# Frame = 4-byte LE length + protobuf payload

function Read-Exact {
    param([IntPtr]$Socket, [int]$Len)
    $buf = New-Object byte[] $Len
    $offset = 0
    while ($offset -lt $Len) {
        $chunk = New-Object byte[] ($Len - $offset)
        $n = [HvSock.Native.Win]::recv($Socket, $chunk, $chunk.Length, 0)
        if ($n -le 0) { throw "recv returned $n; WSA $([HvSock.Native.Win]::WSAGetLastError())" }
        [Array]::Copy($chunk, 0, $buf, $offset, $n)
        $offset += $n
    }
    return ,$buf
}

function Write-All {
    param([IntPtr]$Socket, [byte[]]$Data)
    $offset = 0
    while ($offset -lt $Data.Length) {
        $rem = $Data.Length - $offset
        $chunk = New-Object byte[] $rem
        [Array]::Copy($Data, $offset, $chunk, 0, $rem)
        $n = [HvSock.Native.Win]::send($Socket, $chunk, $rem, 0)
        if ($n -le 0) { throw "send returned $n; WSA $([HvSock.Native.Win]::WSAGetLastError())" }
        $offset += $n
    }
}

# 读 Hello（不解析 protobuf，只验长度合法）
$lenBytes = Read-Exact -Socket $sock -Len 4
$helloLen = [BitConverter]::ToUInt32($lenBytes, 0)
Write-Host "Hello frame length: $helloLen"
if ($helloLen -gt 1048576) { throw "Hello too large: $helloLen" }
$helloPayload = Read-Exact -Socket $sock -Len $helloLen
Write-Host "Received Hello ($($helloPayload.Length) bytes)"

# ----- 5. 发 HelloAck（手工编码 protobuf）-----
# 简化：用 PowerShell 直接生成 protobuf wire format。
#
# 字段编码：
#   HelloAck { ok=1(bool) reason=2(string) device=3(DeviceDescribe) }
#   DeviceDescribe { vendor_id=1 device_id=2 class_code=3 revision=4
#                    subsystem_vendor=5 subsystem_device=6
#                    bars=7(repeated BarInfo) msix_count=8 ... }
#   BarInfo { index=1 size=2 kind=3 prefetchable=4 }
#
# protobuf wire types: 0=varint, 2=length-delimited

function ProtoVarint {
    param([uint64]$Value)
    $bytes = @()
    while ($Value -ge 128) {
        $bytes += [byte](($Value -band 0x7f) -bor 0x80)
        $Value = $Value -shr 7
    }
    $bytes += [byte]$Value
    return ,$bytes
}

function ProtoTagVarint {
    param([int]$FieldNum, [uint64]$Value)
    $tag = ([uint32]$FieldNum -shl 3) -bor 0  # wire type 0
    return (ProtoVarint $tag) + (ProtoVarint $Value)
}

function ProtoTagLengthDelimited {
    param([int]$FieldNum, [byte[]]$Bytes)
    $tag = ([uint32]$FieldNum -shl 3) -bor 2  # wire type 2
    return (ProtoVarint $tag) + (ProtoVarint $Bytes.Length) + $Bytes
}

# BarInfo: index=0, size=4096, kind=MMIO_32(0), prefetchable=false
$bar = (ProtoTagVarint 1 0) + (ProtoTagVarint 2 4096) + (ProtoTagVarint 3 0)
# DeviceDescribe
$dev = (ProtoTagVarint 1 0x1414) + (ProtoTagVarint 2 0xc0de) + (ProtoTagVarint 3 0x010802) +
       (ProtoTagVarint 4 1) + (ProtoTagLengthDelimited 7 $bar) + (ProtoTagVarint 8 1)
# HelloAck
$ack = (ProtoTagVarint 1 1) + (ProtoTagLengthDelimited 3 $dev)

# Frame: 4-byte LE length + payload
$ackBytes = [byte[]]$ack
$frame = [BitConverter]::GetBytes([uint32]$ackBytes.Length) + $ackBytes
Write-All -Socket $sock -Data $frame
Write-Host "Sent HelloAck ($($ackBytes.Length) bytes payload)"

# ----- 6. 主循环 -----
Write-Host "Entering main loop (responds 0 to any MMIO read; ignores writes)..."
while ($true) {
    try {
        $lenBytes = Read-Exact -Socket $sock -Len 4
        $reqLen = [BitConverter]::ToUInt32($lenBytes, 0)
        if ($reqLen -gt 1048576) { throw "frame too large: $reqLen" }
        $reqBytes = Read-Exact -Socket $sock -Len $reqLen
        # 不完整解析 ToHost；任何 frame 都回一个 MmioReadResult value=0，seq=0
        # （真实场景需要 protobuf 解析 seq）
        $resp = (ProtoTagVarint 1 0) + (ProtoTagLengthDelimited 11 (ProtoTagVarint 1 0))
        $respBytes = [byte[]]$resp
        $frame = [BitConverter]::GetBytes([uint32]$respBytes.Length) + $respBytes
        Write-All -Socket $sock -Data $frame
    } catch {
        Write-Host "Session ended: $_"
        break
    }
}

[HvSock.Native.Win]::closesocket($sock) | Out-Null
Write-Host "Done."
