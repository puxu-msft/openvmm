# vmrs_log_scanner

Hyper-V `.vmrs` (saved-state) 中 RAM blocks 的字符串扫描器，用以诊断 OpenHCL
VTL2 早期 panic / 不响应 diag_server 等无 stdout 场景。

## 为什么需要这个工具

Path C（生产 Hyper-V）下，VTL2 早期日志只能通过 `OPENHCL_BOOT_LOG=com3`
输出到 COM3，但 Win11 26200 stock 不支持 Hyper-V Gen2 COM3 (仅 Insider
Canary ≥27813)。`OPENHCL_BOOT_LOG=memory` 则只写入 VTL2 RAM 的固定
buffer，需要事后从 saved-state 抽出。

ohcldiag-dev 也无法连通时（HCS state="Created" / vsock listener 未注册），
这是唯一无侵入的诊断手段。

## 用法

### 1. 从 VM 拿 saved-state

```powershell
# Hyper-V VM 保存（自动写 .vmrs）
Save-VM pcie-remote-exp

# 找 .vmrs 文件位置
$vm = Get-VM pcie-remote-exp
ls "$($vm.Path)\Virtual Machines\*.vmrs"
```

### 2. 扫描

```bash
# 默认扫所有 RamBlock，print 长度 ≥8 的 ASCII run
cargo run --release -- --path /mnt/c/temp/dump.vmrs

# 只看 boot_logger / panic / pcie_remote 相关
cargo run --release -- --path /mnt/c/temp/dump.vmrs \
  --needle '[INFO],[WARN],[ERROR],PANIC,panic,pcie_remote,underhill,diag_server'

# 只扫前 64 个 block (限速)
cargo run --release -- --path /mnt/c/temp/dump.vmrs --max-blocks 64 --verbose
```

## 输出示例

```
[block 0 @ 0x1000] [INFO] openhcl_boot starting
[block 0 @ 0x1000] [ERROR] panic at src/main.rs:42 - vmbus channel not found
[block 0 @ 0x1000] [WARN] pcie_remote: TCP handshake timeout; device absent
```

## 限制

- 不解析 partition_state / register state；只读 `/savedstate/RamBlockN`
- 不去重；boot_logger 的 ring buffer 可能多次出现同一字符串
- 大 RAM VM 扫整个 RAM 可能花数秒到数十秒（顺序 read，无并行）
- ASCII only（UTF-8 内的 ASCII run）；中文/emoji log 不抽出
- VTL2 RAM 与 VTL0 RAM 在 .vmrs 中混合（OpenHCL paravisor 模式），需要
  按 marker 文本（"openhcl"、"underhill"、"pcie_remote" 等）过滤

## 关联实现

- `support/hvs_file` —— `.vmrs` / `.vmcx` / `.vsv` 共用的 KV 格式读写
- `vmm_core/hyperv_dump/src/vmrs_writer.rs` —— 写 `.vmrs` 的对偶；定义了
  `/savedstate/RamMemoryBlockN` (metadata) + `/savedstate/RamBlockN` (1 MiB 数据)
- `openhcl/openhcl_boot/src/boot_logger.rs` —— OpenHCL VTL2 boot_logger
  写入 in-memory StringBuffer (string_page_buf 格式)
