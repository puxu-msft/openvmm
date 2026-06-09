# Runbook — host-root 阻塞项的用户操作步骤

> 这些 Tier 1 项**必须由你（有 sudo / 能装 kernel module / 有真 nvme-cli 的人）
> 跑几条命令**，我（agent）无法在无人值守下做。每项给出：**① 我已就绪的代码状态
> ② 你要跑的精确命令 ③ 把什么贴回来**，我据此在下个会话诊断/迭代（真互通通常要
> 迭代几轮修 wire 不匹配——这正是做真互通的意义）。
>
> WSL2 前置：`nvme-cli` + `nvme-tcp` / `nvme-auth` 内核模块（[[nvme-of-tcp-real-linux-interop-milestone]]
> 验过 WSL2 kernel 6.6 已内置）。

---

## 1. dhchap-4 — 真 nvme-cli DH-HMAC-CHAP 互通（Tier 1 HIGH）

**代码状态**：DHCHAP 4-message spec §8.13.5 wire + simplified wire 两路都过 lib test
+ Python harness（自家算法对自家算法）。**缺的就是真 nvme-cli 这一第三方 oracle**。

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
  -n nqn.2026-06.io.openhcl:nvme.userspace \
  --hostnqn "$HOSTNQN" \
  --dhchap-secret "$DHCHAP_KEY"
sudo nvme list
sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace
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

**目标 NQN**：`nqn.2026-06.io.openhcl:nvme.userspace`（fabric.rs:303，固定）。

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
**步骤**：跑 §1 的 connect 后 `sudo nvme disconnect -n nqn.2026-06.io.openhcl:nvme.userspace`，
贴回 target 日志（应见 Disconnect capsule 处理 + 连接干净关闭）+ `nvme list`（设备消失）。

---

## 回到无人值守

以上任一项贴回输出后，下个会话我直接据真 oracle 诊断/修/加回归测试——这就是
[[nvme-of-tcp-real-linux-interop-milestone]] 当初修 9 个 wire blocker 的同款循环。
纯代码项（nvme_of fused fabric / Python CHAP wire conformance）不需要你，见
[2026-06-09-fused-fabric-and-chap-conformance.md](/usnvmemu/crates/nvme_of_tcp_target/docs/plans/2026-06-09-fused-fabric-and-chap-conformance.md)。
