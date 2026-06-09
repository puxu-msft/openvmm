# Runbook — host-root 阻塞项的用户操作步骤

> 这些 Tier 1 项**必须由你（有 sudo / 能装 kernel module / 有真 nvme-cli 的人）
> 跑几条命令**，我（agent）无法在无人值守下做。每项给出：**① 我已就绪的代码状态
> ② 你要跑的精确命令 ③ 把什么贴回来**，我据此在下个会话诊断/迭代（真互通通常要
> 迭代几轮修 wire 不匹配——这正是做真互通的意义）。
>
> WSL2 前置：`nvme-cli` + `nvme-tcp` 内核模块（已内置）。**host 侧 CHAP（§1）另需
> `CONFIG_NVME_AUTH`——WSL2 默认内核没编**，见 §1 内核前置。

---

## 0. 纯-4K Format + IO 真 nvme-cli 互通（Tier 1 HIGH，★最高优先 / 本内核即可做）

> **✅ 2026-06-09 实测达成**：`sudo bash scripts/wsl_4k_format_interop.sh` 在 WSL2 kernel
> 6.6.114 + nvme-cli 2.8 上 **PASS** —— `id-ns` 确认 LBAF[2] "Data Size: 4096 (in use)"，
> 8×4K distinct-block round-trip 全等，**独立 backing-file oracle 确认数据落在 5\*4096
> (=0xc5) 而非 5\*512 (=0xa0)**。纯-4K Format+IO 真 nvme-cli 互通成为继 vfio 真 QEMU、
> nvme-of plaintext 之后又一条第三方 oracle 验证（本会话 ship 的 firmware 扇区感知偏移
> 代码由真内核背书，非仅自家测试）。

**为什么先做这个**：① 验的是本会话刚 ship 的新代码（`--allow-format` Format NVM →
LBAF[2] 纯-4K + 扇区感知 `slba*4096` IO 偏移）——目前只过 lib test + Python harness
（自家对自家），**缺真 nvme-cli 第三方 oracle**。② 走 plaintext，**不碰
`CONFIG_NVME_AUTH`，当前 WSL2 内核 6.6.114 直接能跑，无需重编内核**（不像 §1 CHAP）。
③ 已封装成一键自断言脚本，你只跑一条命令 + 贴回日志。

**代码状态**：firmware `controller/io.rs` + `completion.rs` 扇区感知（`1<<lbads`）；
fabric `async_session.rs` 扇区感知 chunking；Format 经 `--allow-format` opt-in 放行
（`dispatch_plan.rs::decide_admin_blocked_opc`）。lib test `pure_4k_io_round_trip_mixed_ns` /
`pure_4k_write_zeroes_offset` / `pure_4k_copy_offset` 全 revert-verified。

**① 启动 target**（普通用户，**务必带 `--allow-format`**）：
```bash
cd usnvmemu/crates/nvme_of_tcp_target
truncate -s 64M /tmp/ns1.img
cargo run --bin nvme_of_tcp_target -- \
  --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img --allow-format
```

**② 跑一键脚本**（另一个终端，**需 sudo**）：
```bash
sudo BACKING=/tmp/ns1.img bash scripts/wsl_4k_format_interop.sh
```
脚本做：connect → id-ns(前) → `nvme format --lbaf=2`（LBAF[2]=纯 4K）→ id-ns(后，
断言 in-use=4096) → 8×4K distinct-block round-trip cmp → **★非零 start-block 写后直读
backing file 断言数据落在 `5*4096` 而非 `5*512`**（独立 oracle，纯 round-trip 测不出
"读写都用错 ×512"的自洽 bug）→ disconnect。末尾打 `✅ PASS` / `❌ FAIL(n)`。

**③ 贴回给我**：
- 脚本 stdout 全部（尤其末尾 PASS/FAIL 行 + 任何 `!! ASSERT FAIL`）。
- `/tmp/host4k_*.log` 全部（connect / format / idns_before / idns_after / write / read /
  offset_oracle / disconnect）。
- 失败时另加 `sudo dmesg | tail -40`。

**已知风险**（我会据贴回迭代，这正是真互通的意义）：
- `nvme format --lbaf=2` 若报 invalid → 可能内核对 fabric NS 的 Format 支持差异，或我
  Identify NS 的 LBAF 表 byte layout 与 nvme-cli 解析不符（手算 offset 的老坑），据
  `host4k_idns_after.log` 对齐。
- backing[20480]=0x00（非 c5）→ target 的 Flush 未把用户态缓冲落盘，我加 fsync 或改
  write-through。
- 设备节点是普通文件而非 `brw-` → 旧会话 `echo >` 垃圾，脚本会 `rm` 后提示重连。
- STEP 1 connect 打印 `Failed to write to /dev/nvme-fabrics: Invalid argument`（2026-06-09
  实测见过）→ 多半是**已有同 subsysnqn 的 controller 残留**（上轮没 disconnect）；脚本按
  subsysnqn 找到既存 controller 继续，asserts 仍 PASS。要干净复现先
  `sudo nvme disconnect -n nqn.2014-08.org.nvmexpress:teaching:disk`。

---

## 1. dhchap-4 — 真 nvme-cli DH-HMAC-CHAP 互通（Tier 1 HIGH）

**代码状态**：DHCHAP 4-message spec §8.13.5 wire + simplified wire 两路都过 lib test
+ Python harness（自家算法对自家算法）。**缺的就是真 nvme-cli 这一第三方 oracle**。

> **⛔ 内核前置（2026-06-09 实测发现，硬阻塞）**：host 侧 DH-HMAC-CHAP connect 需要
> 内核编了 **`CONFIG_NVME_AUTH`**。先查：
> ```bash
> (zcat /proc/config.gz 2>/dev/null || cat /boot/config-$(uname -r)) | grep NVME_AUTH
> ```
> - `CONFIG_NVME_AUTH=y`（或 `=m`）→ 可继续 §1。
> - `# CONFIG_NVME_AUTH is not set` → **本内核做不了 host CHAP connect**（nvme-cli 报
>   `option "dhchap_secret" ignored` + `/dev/nvme-fabrics: Invalid argument`）。
>   **当前 WSL2 内核 6.6.114.1-microsoft 正是这种**（只编了 NVME_TCP/FABRICS，没 NVME_AUTH）。
>   真修路径见本节末"内核阻塞的出路"。注意：之前以为"WSL2 已开 nvme-auth"——那指的是
>   **target 侧** `nvme_auth_derive_tls_psk` 的 EXPORT_SYMBOL（nvme-core），与 **host 侧
>   connect auth**（CONFIG_NVME_AUTH）是两回事，别混。

**① 启动 target**（普通用户，无需 sudo）：
```bash
cd usnvmemu/crates/nvme_of_tcp_target
truncate -s 64M /tmp/ns1.img
# HOSTNQN 自定；HEXSECRET = 任意 ≥ 32 字节的 hex（64 hex 字符 = 32 字节）
HOSTNQN='nqn.2014-08.org.nvmexpress:uuid:test-host'
HEXSECRET=$(head -c 32 /dev/urandom | xxd -p -c 64)
echo "HEXSECRET=$HEXSECRET"   # 记下，要和下面 DHHC-1 同源
cargo run --bin nvme_of_tcp_target -- \
  --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img \
  --host-secret "${HOSTNQN}=${HEXSECRET}"
```
> flag 是 `--listen`（不是 `--addr`）；`--host-secret` 的 HEX 解码后须 ≥ 32 字节
> （上面 32 字节裸 secret = 64 hex 字符，正好）。加 `RUST_LOG=debug` 前缀看 CHAP 帧。

**② 生成与 HEXSECRET 同源的 DHHC-1 key**（nvme-cli `--dhchap-secret` 要
`DHHC-1:<hmac>:base64(key‖crc32_le):` 格式——**含 CRC-32 后缀**，不能手写裸 base64；
用官方 `nvme gen-dhchap-key` 才会带正确 CRC + 长度）：
```bash
# **review 修正（CRITICAL）**：用 --hmac 0（identity，key == 裸 secret），这样
# nvme-cli 解码去 CRC 后的 key 恰好 == target 的 --host-secret 裸 hex（两端同源）。
# --secret 是 HEX（不是 ascii）。绝不要手写 `xxd|base64`（缺 CRC，connect 必拒）。
DHCHAP_KEY=$(nvme gen-dhchap-key --hmac 0 --secret "$HEXSECRET")
echo "$DHCHAP_KEY"                          # 形如 DHHC-1:00:<base64(secret‖crc32)>:
nvme check-dhchap-key --key "$DHCHAP_KEY"   # 自验：应打印 key 合法
```
> 为何 `--hmac 0`：`--hmac 1/2/3` 会把 key 变换成 `HMAC(secret, hostnqn‖seed)`，
> **不可逆**，target 的 `--host-secret` 裸 hex 就对不上了。hmac=0 时 DHHC-1 仅是
> `base64(裸 secret ‖ CRC)`，解码去 CRC == 裸 secret，两端一致。
> （CHAP **响应**仍用 SHA-256 HMAC 算 response，与 key 的 hmac_id 是两回事。）

**③ 连接**（另一个终端，**需 sudo**）：
```bash
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 \
  -n nqn.2014-08.org.nvmexpress:teaching:disk \
  --hostnqn "$HOSTNQN" \
  --dhchap-secret "$DHCHAP_KEY"
sudo nvme list
sudo nvme disconnect -n nqn.2014-08.org.nvmexpress:teaching:disk
```
> **secret 卫生**：`--host-secret` / `--dhchap-secret` 出现在命令行会进 `ps aux` +
> shell history（教学/loopback 可接受）。要避免：命令前置一个空格 +
> `export HISTCONTROL=ignorespace`；生产应改用 `/etc/nvme/hostkey` 文件或 keyring。

**③ 贴回给我**：
- target 终端的全部日志（尤其 `RUST_LOG=debug` 时 DHCHAP NEGOTIATE/CHALLENGE/REPLY/SUCCESS 各帧）。
- `nvme connect` 的输出（成功 = "connecting to device" / 失败 = errno + dmesg）。
- 失败时：`sudo dmesg | tail -40`（kernel nvme-auth 侧的 reject 原因）。

**已知风险**（我会据贴回迭代）：教学版 CHAP transcript 用简单 `||` 串接而非 spec
wire format（dhchap.rs:75 注释）；真 nvme-cli 走 spec §8.13.5 4-message wire——若
response 不匹配，是这里的 transcript 串法差异，我会对齐 spec wire。

**目标 NQN**：`nqn.2014-08.org.nvmexpress:teaching:disk`（cmd.rs:674 Identify Controller SUBNQN，固定；内核日志确认）。

### 内核阻塞的出路（CONFIG_NVME_AUTH 缺失时）

**先验证其余栈正常（plaintext，不需 CONFIG_NVME_AUTH）**——target **不带** `--host-secret`
重启，再普通 connect：
```bash
# target 终端：去掉 --host-secret
cargo run --bin nvme_of_tcp_target -- --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img
# host 终端（sudo）：
sudo nvme connect -t tcp -a 127.0.0.1 -s 4420 -n nqn.2014-08.org.nvmexpress:teaching:disk \
  --hostnqn nqn.2014-08.org.nvmexpress:uuid:test-host
sudo nvme list        # 应见 /dev/nvmeXn1
sudo nvme disconnect -n nqn.2014-08.org.nvmexpress:teaching:disk
```
plaintext 通 = 传输/Connect/Identify 栈都好，**只差 host CHAP 这一环卡内核**。

> **✅ 2026-06-09 实测达成**：plaintext connect → 内核 `creating 4 I/O queues` +
> `new ctrl teaching:disk` + `id-ns` 正确 + `dd`/`nvme read` 真 IO 成功。**NVMe-oF TCP
> 真 host 全栈 IO 互通达成**（vfio 真 QEMU 之外第 2 条接入）。
>
> **坑（实测踩过）**：若 `ls -l /dev/nvme0n1` 显示是**普通文件**（`-rw-r--r--` 而非块
> 设备 `brw-`），是旧会话 `echo > /dev/nvme0n1` 误重定向留下的垃圾，会挡住真块设备 →
> `nvme list` 报 `Failed to open ns nvme0n1, errno 22`。修：`sudo rm -f /dev/nvme0n1`
> 后断开重连。成功判据：`ls -l /dev/nvme0n1` 是 `brw-` 块设备 +
> `sudo dd if=/dev/nvme0n1 of=/dev/null bs=4k count=8` 成功。

**要真做 host CHAP interop，三选一**：
1. **重编 WSL2 内核开 `CONFIG_NVME_AUTH=y`**：clone `microsoft/WSL2-Linux-Kernel`
   对应 tag，`make menuconfig` 开 `Device Drivers → NVME Support → NVM Express over
   Fabrics In-Band Authentication`，编出 `bzImage` → `.wslconfig` 的 `kernel=` 指过去
   → `wsl --shutdown` 重启。（半天工作；最彻底。）
2. **用一台带 CONFIG_NVME_AUTH 的发行版内核**（多数主线 distro kernel 默认开）的真机/VM
   跑 host 侧 connect，target 仍在这。
3. **暂用 Python CHAP harness 作 oracle**（`scripts/interop_py/chap4_spec_wire_e2e.py`，
   已跨进程实证 §8.13.5 wire）——非真 nvme-cli，但比 lib test 独立。真 nvme-cli 留到
   有 CONFIG_NVME_AUTH 内核时。

**贴回**（任一路径）：plaintext connect 结果 + 若走路径 1/2 的 CHAP connect 输出 +
target CHAP 帧日志。

---

## 2. tls-psk-kernel-vector — 锚定 kernel TLS PSK 派生（Tier 1 HIGH）

**代码状态**：`src/tls_psk.rs` 有 13 个 deterministic test，但**全 self-consistent**
（对自身一致，没 anchor 到 Linux 真实输出）——任何 silent algorithm drift 不会被发现。
详 [2026-06-06-phase-v-followup-tls-psk-survey.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md)。

**你要做**：写一个 ~50 LOC out-of-tree kernel module，调
`nvme_auth_derive_tls_psk()`（kernel `drivers/nvme/common/auth.c`，已
`EXPORT_SYMBOL_GPL`），dump 5 组 known-good 五元组到 dmesg：
```
(retained_psk, hostnqn, subsysnqn, hash_id, expected_tls_psk_hex)
```

**① 模块骨架**（你写或我代写后你编译装载）：
```c
// kmod 调 nvme_auth_derive_tls_psk(hmac_id, hostnqn, subsysnqn, psk, psk_len, &out)
// 对 3 组 SHA-256 + 2 组 SHA-384 输入，printk 出 hex(out)。
// Makefile: obj-m += vt_tlspsk.o ; make -C /lib/modules/$(uname -r)/build M=$(pwd)
```
（需要我先把完整 kmod + Makefile 写好让你 `make` + `sudo insmod` 吗？告诉我，我下个
会话产出 `scripts/kmod_tlspsk/`。）

**② 贴回给我**：`sudo dmesg | grep vt_tlspsk` 的 5 组五元组 hex。

**我据此做**：把它们硬编码成 `tls_psk.rs::vt_tlspsk_kernel_ci_vector_{1..5}` 回归测试
（之后任何 tls_psk.rs 改动必须先改 vector→CI red→再改实现）。

---

## 3. discovery-multi-portal-real（Tier 2 MEDIUM）

**代码状态**：V7c-fix 改了 CNTRLTYPE，但只测过单 portal。

**① 启动多 portal target**（`--discovery-mode` 让主 `--listen` 端口服务 discovery）：
```bash
cargo run --bin nvme_of_tcp_target -- \
  --listen 127.0.0.1:4420 --backing-file /tmp/ns1.img --discovery-mode \
  --discovery-target-nqn nqn.test:p1 --discovery-target-addr 127.0.0.1:4421 \
  --discovery-target-nqn nqn.test:p2 --discovery-target-addr 127.0.0.1:4422 \
  --discovery-target-nqn nqn.test:p3 --discovery-target-addr 127.0.0.1:4423
```
**② 发现**（需 sudo）：`sudo nvme discover -t tcp -a 127.0.0.1 -s 4420`
**③ 贴回**：discover 输出（期望 3 个 entry，每个 NQN/IP/Port 正确）。

---

## 4. fabric-disconnect-real-interop（Tier 2 MEDIUM）

**代码状态**：V8c Disconnect 只测过 Python e2e。
**步骤**：跑 §1 的 connect 后 `sudo nvme disconnect -n nqn.2014-08.org.nvmexpress:teaching:disk`，
贴回 target 日志（应见 Disconnect capsule 处理 + 连接干净关闭）+ `nvme list`（设备消失）。

---

## 回到无人值守

以上任一项贴回输出后，下个会话我直接据真 oracle 诊断/修/加回归测试——这就是
[[nvme-of-tcp-real-linux-interop-milestone]] 当初修 9 个 wire blocker 的同款循环。
纯代码项（nvme_of fused fabric / Python CHAP wire conformance）不需要你，见
[2026-06-09-fused-fabric-and-chap-conformance.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-09-fused-fabric-and-chap-conformance.md)。
