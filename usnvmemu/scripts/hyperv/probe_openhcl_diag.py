"""Probe OpenHCL VTL2 diag listener over AF_HYPERV with HVSOCKET_HIGH_VTL=1.

Requires Python on Windows (any version, ships with .NET-style sockets).
Does NOT need admin or GUID registration in some cases (HIGH_VTL bypasses
some checks for VTL2-targeted connects per OpenHCL docs).

Usage:
    py probe_openhcl_diag.py <vm-name>

Exit code 0 = TCP-level connect to VTL2 diag port 1 succeeded
                (OpenHCL diag server is alive).
Exit code != 0 = failure with WSA errno (10061 = no listener,
                10049 = address not available, etc.).
"""
import ctypes
from ctypes import wintypes
import subprocess
import sys
import uuid

AF_HYPERV = 34
SOCK_STREAM = 1
HV_PROTOCOL_RAW = 1
HVSOCKET_HIGH_VTL = 8

class SOCKADDR_HV(ctypes.Structure):
    _fields_ = [
        ('Family', wintypes.USHORT),
        ('Reserved', wintypes.USHORT),
        ('VmId', ctypes.c_byte * 16),
        ('ServiceId', ctypes.c_byte * 16),
    ]

def guid_to_bytes(g):
    """Convert a uuid.UUID to .NET byte layout (data1+data2+data3 little-endian)."""
    return g.bytes_le

def main(vm_name, port=1):
    # 1. Get VmId via PowerShell
    r = subprocess.run(['powershell', '-NoProfile', '-Command', f"(Get-VM '{vm_name}').Id.ToString()"],
                       capture_output=True, text=True, timeout=20, check=True)
    vm_id = uuid.UUID(r.stdout.strip())
    print(f"VM Id: {vm_id}")

    # 2. Compute service GUID: port (4 bytes) + facb-11e6-bd58-64006a7986d3
    svc_str = f"{port:08x}-facb-11e6-bd58-64006a7986d3"
    svc_id = uuid.UUID(svc_str)
    print(f"Service Id: {svc_id}")

    # 3. WSAStartup
    ws2_32 = ctypes.WinDLL('ws2_32', use_last_error=True)
    class WSADATA(ctypes.Structure):
        _fields_ = [('wVersion', wintypes.WORD), ('wHighVersion', wintypes.WORD),
                    ('iMaxSockets', wintypes.SHORT), ('iMaxUdpDg', wintypes.SHORT),
                    ('lpVendorInfo', ctypes.c_void_p), ('szDescription', ctypes.c_char * 257),
                    ('szSystemStatus', ctypes.c_char * 129)]
    wsadata = WSADATA()
    ws2_32.WSAStartup(0x0202, ctypes.byref(wsadata))

    INVALID_SOCKET = ctypes.c_void_p(-1).value
    ws2_32.WSASocketW.restype = ctypes.c_void_p
    ws2_32.WSASocketW.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                  ctypes.c_void_p, ctypes.c_uint, ctypes.c_uint]
    sock = ws2_32.WSASocketW(AF_HYPERV, SOCK_STREAM, HV_PROTOCOL_RAW, None, 0, 0)
    if sock == 0 or sock == INVALID_SOCKET:
        print(f"WSASocket failed: WSA {ctypes.get_last_error()}")
        return 2

    # 4. set HVSOCKET_HIGH_VTL = 1
    high_vtl = ctypes.c_uint32(1)
    ws2_32.setsockopt.restype = ctypes.c_int
    ws2_32.setsockopt.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int,
                                  ctypes.c_char_p, ctypes.c_int]
    r = ws2_32.setsockopt(sock, HV_PROTOCOL_RAW, HVSOCKET_HIGH_VTL,
                          ctypes.cast(ctypes.pointer(high_vtl), ctypes.c_char_p), 4)
    if r != 0:
        print(f"setsockopt(HVSOCKET_HIGH_VTL) failed: WSA {ctypes.get_last_error()}")
        ws2_32.closesocket(sock)
        return 3

    # 5. connect
    addr = SOCKADDR_HV()
    addr.Family = AF_HYPERV
    addr.Reserved = 0
    ctypes.memmove(addr.VmId, guid_to_bytes(vm_id), 16)
    ctypes.memmove(addr.ServiceId, guid_to_bytes(svc_id), 16)

    ws2_32.connect.restype = ctypes.c_int
    ws2_32.connect.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int]
    r = ws2_32.connect(sock, ctypes.byref(addr), ctypes.sizeof(addr))
    if r == 0:
        print(f"*** CONNECT SUCCESS to VTL2 port {port}! OpenHCL diag is alive.")
        ws2_32.closesocket(sock)
        return 0
    else:
        err = ctypes.get_last_error()
        print(f"connect failed: WSA {err}")
        if err == 10061:
            print("  → 10061 ECONNREFUSED: no listener at port (or GUID not registered)")
        elif err == 10049:
            print("  → 10049 EADDRNOTAVAIL: VmId or ServiceId mismatch")
        elif err == 10060:
            print("  → 10060 ETIMEDOUT")
        elif err == 10013:
            print("  → 10013 EACCES: ACL denied (service GUID not registered or ACL restricts)")
        ws2_32.closesocket(sock)
        return 4

if __name__ == '__main__':
    vm = sys.argv[1] if len(sys.argv) > 1 else 'pcie-remote-exp'
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    sys.exit(main(vm, port))
