# NVMe-oF TCP 加密机制探索 + 内核构建交接 plan

> **状态**：交接 plan（2026-06-14 产出，供新会话执行）。所有"现状"已 file:line 核验（见 §6）。
> **混合详度**：Phase A/B 标 **execution-ready**（可直接做）；Phase C **kernel-build 详化**（用户-gated
> 真机步骤 + 会话侧代码）；Phase D **roadmap**（卡上游，仅 watch）。
> **执行纪律**：本项目方法论强制——每 phase 末过 `ecc:rust-reviewer`/对应 reviewer + revert-verify；
> crypto 必 WebFetch 内核源核字段顺序（LESSONS §12）；self-consistent 测试不算数，要第三方 oracle
> （LESSONS §2/§20）；commit hunk 隔离（共享树多会话）；改完同步 ROADMAP 矩阵 + RUNBOOK。

---

## 0. 目标

探索并**夯实** nvme-of-tcp 的三套加密机制的**正确性可信度**，重点闭合"self-consistent ≠
spec-conformant"缺口（LESSONS §2）。用户明确要"如何构建新内核"——内核是其中两件的解锁前置，
本 plan 详解内核重编 + kmod 构建。

**一句话现状**：TLS/mTLS/NQN-binding 全栈 shipped；DHCHAP spec 4-msg framing 已对**但 HMAC
transcript 仍是教学简化 `||` 串接**（非 spec §8.13.5.3）；TLS-PSK 只有 deterministic crypto、
**未接 TLS 握手**（卡上游 rustls）、且**全 self-consistent**（无第三方 reference vector）。

---

## 1. 现状（已核验，§6 有锚点）

### 1.1 三套机制

| 机制 | 文件 | 状态 | 测试 | self-consistent? |
|---|---|---|---|---|
| **TLS / mTLS** | `src/tls.rs` + `tls_identity.rs` | ✅ rustls 0.23(ring)，server-auth + mTLS(WebPkiClientVerifier) + NQN↔cert binding | `vt_tls_*`/`vt_mtls_*`/`vt_auth*` + Python `tls_smoke/mtls_smoke/nqn_cert_binding.py` | 半独立（Python `ssl` 是第三方栈） |
| **DHCHAP** | `src/dhchap.rs` | ⚠ simplified + spec 4-msg framing **都通**；但 `compute_response` = `HMAC(secret, challenge‖hostnqn‖subnqn)` 简单 `||`，**非 spec §8.13.5.3 transcript** | `vt_dhchap2/3/3w/4_*` + Python `chap_e2e/chap4_spec_wire_e2e.py`(9 scenario) | **crypto 自洽**（两端都用本仓 `compute_response`） |
| **TLS-PSK (TP-8011)** | `src/tls_psk.rs` | ⚠ deterministic crypto 全栈（generate/derive PSK，HKDF-Expand-Label，SHA-256/384）**但未接 TLS 握手**（rustls 0.23 无 external-PSK 公开 API） | 13 个 deterministic 单测 | **纯 self-consistent**（无 kernel vector，§2 点名差距） |

### 1.2 两个核心缺口

- **G1（crypto transcript）**：DHCHAP 即使走 spec 4-msg wire，`async_session.rs` 最终仍调
  `compute_response()` 的简化 `||`，**不是 spec §8.13.5.3 transcript**。→ 真 Linux nvme-cli 用
  spec transcript 算 HMAC，**自家通 ≠ 真互通**（LESSONS §2 第一坑）。**纯代码可修**（= V-spec-strict S1）。
- **G2（self-consistent crypto 无第三方 oracle）**：tls_psk.rs（13 测全自洽）+ DHCHAP crypto 都缺
  第三方 reference vector。任何 HKDF-Expand-Label 字段顺序漂移（LESSONS §2 第二坑：错半字节 kernel
  拒）silent 不报。→ 需**档2 frozen-vector**（kernel dump，= ADR-013 矩阵 NVMe-oF 行档2"待 dump"）。

### 1.3 与 ADR-013 矩阵的接口

NVMe-oF TCP 行（[ROADMAP](/usnvmemu/docs/ROADMAP.md) Tier-1 元项矩阵）：
- 档1 = Rust wire e2e（`vt_dhchap4` 等）【framing 手搓 indep / crypto 自洽】。
- 档2 frozen-vector = **tls-psk kernel vector（待 dump）** ← 本 plan Phase B 填。
- 档3 = dhchap-4 真 `nvme connect` ← 本 plan Phase C（卡内核）。

---

## 2. 阶段化路线

### Phase A — CHAP spec §8.13.5.3 transcript（**execution-ready，纯代码无人值守**，HIGH）

**这是 dhchap-4 真互通的隐性纯代码前置**——没它，即使有 CONFIG_NVME_AUTH 内核，真 nvme-cli 也因
transcript 不同而 HMAC 对不上拒连。ROADMAP/multi-month 明确建议提前单做（`ROADMAP.md:91-92`）。

**做什么**：
1. **WebFetch 内核源**（LESSONS §12，**crypto 必做**）：`raw.githubusercontent.com/torvalds/linux/master/drivers/nvme/host/auth.c` +
   `drivers/nvme/common/auth.c`，抽 `nvme_auth_*` 的**真 transcript 构造**（DH-HMAC-CHAP reply
   transcript = spec §8.13.5.3：`challenge ‖ seqnum(4B LE?) ‖ T_ID(2B) ‖ scc(1B) ‖ "HostHost" ‖
   hostnqn ‖ 0x00 ‖ subnqn` 之类——**以内核源为准核对字节顺序/分隔符**，别照本 plan 记忆）。
2. **新增 spec-transcript compute**（不删旧 `compute_response`，加 `compute_response_spec`）：按内核
   核实的 transcript 算 HMAC-SHA256。anchor 测试用**已知输入→已知输出**（先从内核源/spec 例子取，
   理想是 Phase B 的 kernel vector 反哺）。
3. **接进 4-msg wire 路径**：`async_session.rs` 的 Spec4Msg 分支 `verify_host_response` 改调
   spec transcript（simplified wire 路径保留旧的，兼容老测试）。
4. **Python harness 同步**：`chap4_spec_wire_e2e.py` 的 client 侧 transcript 也改 spec，跨进程实证。

**Acceptance**：新 anchor 测试绿；`vt_dhchap4_spec_wire_e2e.rs` + Python 9 scenario 全绿；
revert-verify（改 transcript 一个字节 → anchor 红）。过 rust-reviewer。
**风险/gate**：transcript 字节顺序是 §2 第一坑核心——**必以 WebFetch 的内核源为唯一判据**，不自洽自证。

### Phase B — tls-psk-kernel-vector：档2 frozen-vector（**execution-ready 代码 + 用户-gated 一次 root 跑**，HIGH）

闭 G2（tls_psk self-consistent）+ 填 ADR-013 NVMe-oF 档2。**只需 target 侧
`nvme_auth_derive_tls_psk` EXPORT_SYMBOL（WSL2 nvme-core 已有），不需 CONFIG_NVME_AUTH、不需重编内核**
——故比 Phase C 易解，建议**先于 C 做**。

**会话侧（execution-ready，产出给用户跑）**：
1. 写 `scripts/kmod_tlspsk/` —— ~50 LOC out-of-tree kmod + Makefile：
   ```c
   // 调 nvme_auth_derive_tls_psk(hmac_id, hostnqn, subsysnqn, psk, psk_len, &out)
   //   （kernel drivers/nvme/common/auth.c，EXPORT_SYMBOL_GPL）
   // 对 5 组输入(3×SHA-256 hmac_id=1 + 2×SHA-384 hmac_id=2) printk hex(out)。
   // Makefile: obj-m += vt_tlspsk.o ; make -C /lib/modules/$(uname -r)/build M=$(pwd)
   ```
   **先 WebFetch 内核 `nvme_auth_derive_tls_psk` 真签名**（参数序/类型可能随版本变）再写。
2. 给用户**精确 build+load 指令**（见 §3.2）。
**用户侧（root，一次性）**：`make` → `sudo insmod vt_tlspsk.ko` → `sudo dmesg | grep vt_tlspsk` 贴回
5 组 `(retained_psk, hostnqn, subsysnqn, hash_id, expected_tls_psk_hex)`。
**会话侧收口**：硬编码成 `tls_psk.rs::vt_tlspsk_kernel_ci_vector_{1..5}_sha{256,384}` 回归（provenance:
kernel 版本 + 日期，refresh owner，同 ADR-013 档2 纪律）；revert-verify（改 HKDF 一字节 → vector 红）；
ROADMAP 矩阵 NVMe-oF 档2 ⛔→✅。**这反哺 Phase A 的 anchor**（kernel 真值）。

### Phase C — dhchap-4 真 host interop（**kernel-build 详化，用户-gated**，HIGH）= ADR-013 档3

依赖 Phase A（transcript 对了真 nvme-cli 才连得上）。需 `CONFIG_NVME_AUTH` 内核——**这是用户要的
"构建新内核"主线**，详 §3.1。

**流程**：用户按 §3.1 重编 WSL2 内核（或用 distro VM）→ 会话产出 target 启动 + `nvme connect
--dhchap-secret` 精确命令（RUNBOOK §1 已有，会话据 Phase A 更新）→ 用户跑、贴回 → 会话据真 oracle
迭代（同当年修 9 个 wire blocker 的循环）。**plaintext connect（不卡内核）先验栈**。

### Phase D — tls-psk-rustls-wire（**roadmap，卡上游**，不在本会话主线）

rustls 0.23 无 external-PSK 公开 API（issue#174 open；PR#2424/#2524 closed-no-merge）。
**不可在本会话解锁**。仅：① watch 上游；② 可选 SpiderOak `rustls` fork git-dep **实验**（survey 路径 C，
隔离在 experiments/ 不进主线）。tls_psk.rs 的 deterministic crypto 已就绪，等 API 即接 `build_acceptor_with_psk`。

---

## 3. 内核构建详解（用户明确诉求）

> **两件内核工作目标不同，别混（三处 docs 都强调）**：
> - **Phase C 要 `CONFIG_NVME_AUTH`**：host 侧 `nvme connect --dhchap-secret` 的 in-band auth。WSL2 **没编** → 须**重编内核**或换内核。
> - **Phase B 要 `nvme_auth_derive_tls_psk` EXPORT_SYMBOL**：target 侧 nvme-core 符号，WSL2 **已有** → **只编一个 out-of-tree kmod**，不动内核。

### 3.1 重编 WSL2 内核开 `CONFIG_NVME_AUTH`（Phase C 前置，~半天）

**先查现状**（确认确实缺）：
```bash
(zcat /proc/config.gz 2>/dev/null || cat /boot/config-$(uname -r)) | grep -E 'NVME_AUTH|NVME_TCP|NVME_FABRICS'
uname -r   # 预期 6.6.114.1-microsoft-standard-WSL2，含 NVME_TCP/FABRICS、缺 NVME_AUTH
```

**重编步骤**（出路 1，最彻底）：
1. `git clone --depth 1 --branch linux-msft-wsl-<对应版本> https://github.com/microsoft/WSL2-Linux-Kernel.git`
   （branch/tag 对齐 `uname -r`；查 tags 选最接近的）。
2. 取当前配置作基线：`zcat /proc/config.gz > .config`（或仓库 `Microsoft/config-wsl`）→ `make olddefconfig`。
3. 开 auth：`make menuconfig` → `Device Drivers → NVME Support → NVM Express over Fabrics
   In-Band Authentication`（= `CONFIG_NVME_AUTH=y`）。或直接 `scripts/config --enable NVME_AUTH`。
   （连带确认 `CONFIG_NVME_TCP=y`、`CONFIG_KEYS=y`、`CONFIG_CRYPTO_*`(SHA256/HMAC/DH) 已开。）
4. 编：`make -j$(nproc) bzImage`（WSL2 内核构建依赖：`flex bison libssl-dev libelf-dev bc dwarves`——
   缺 flex/bison 是常见坑，先 `sudo apt install`）。产物 `arch/x86/boot/bzImage`。
5. 指向：Windows 侧 `%USERPROFILE%\.wslconfig` 加
   ```
   [wsl2]
   kernel=C:\\path\\to\\bzImage   # 把 bzImage 复制到 Windows 可达路径
   ```
6. `wsl --shutdown`（Windows PowerShell）→ 重开 WSL → `uname -r` 确认新内核 + 上面 grep 确认
   `CONFIG_NVME_AUTH=y`。
7. **回归**：重编内核可能影响别的（如 vfio/MSHV 实验）；`CONFIG_MSHV_ROOT` 等若之前靠默认，注意一并保留。

**出路 2（更省事，若有条件）**：带 `CONFIG_NVME_AUTH` 的发行版内核 VM/真机跑 host `nvme connect`，
target 仍在 WSL2。多数主线 distro kernel 默认开 NVME_AUTH。
**出路 3（暂代，不重编）**：Python `chap4_spec_wire_e2e.py` 作 oracle（非真 nvme-cli，但跨进程独立）。

### 3.2 tls-psk dump kmod 构建（Phase B，不重编内核）

```bash
cd usnvmemu/crates/nvme_of_tcp_target/scripts/kmod_tlspsk   # 会话产出
make -C /lib/modules/$(uname -r)/build M=$(pwd) modules      # 需 linux-headers(/lib/modules/$(uname -r)/build 存在)
sudo insmod vt_tlspsk.ko
sudo dmesg | grep vt_tlspsk     # 5 组五元组 hex,贴回
sudo rmmod vt_tlspsk
```
**坑**：WSL2 需 `/lib/modules/$(uname -r)/build`（kernel headers）。若缺，要么装 headers，要么在
§3.1 的内核源树里 `make M=... modules`（同源更稳）。`nvme_auth_derive_tls_psk` 是 `EXPORT_SYMBOL_GPL`
→ kmod 须 `MODULE_LICENSE("GPL")` 才能链接该符号。

---

## 4. 依赖图 + 阻塞分类

```
Phase A (transcript, 纯代码) ──┬──> Phase C (真 nvme connect, 卡内核 CONFIG_NVME_AUTH)
                              │       └─ 用户重编内核(§3.1) 或 distro VM
Phase B (kmod vector, root)  ──┘  └─ 反哺 A 的 anchor + 闭 G2 + 填档2
   └─ 只需 kmod(§3.2),不重编内核 → 建议最先做
Phase D (rustls PSK) ── 卡上游,不可本会话解锁,仅 watch
```

- **纯代码无人值守**：Phase A（transcript）。**最先做**——它解锁 C，且本身修真互通正确性。
- **root + kmod（不重编）**：Phase B。**次先**——闭 self-consistent gap，且其 kernel 真值反哺 A 的 anchor。
- **重编内核（用户半天）**：Phase C 的真 nvme connect。
- **卡上游**：Phase D。不碰。

**建议执行序**：A（纯代码先行，含 WebFetch 内核源）→ B（kmod，会话写+用户一次 root 跑，真值反哺 A）→
C（用户重编内核 + 真 connect，会话迭代）→ D 仅记录 watch。

---

## 5. 方法论 gates（每 phase 强制）

1. **crypto 必 WebFetch 内核源核字段顺序**（LESSONS §12）——transcript / HKDF-Expand-Label 的字节序、
   分隔符、label 前缀（`"tls13 "`）以内核 `auth.c` 为唯一判据，**绝不自洽自证**（§2 两坑都在这）。
2. **第三方 oracle 闭 self-consistent**（LESSONS §2/§20）：kernel vector（B）/ 真 nvme-cli（C）是唯一
   能证 crypto 真对的 oracle；自家两端互验只证"没崩"不证"spec 对"。
3. **revert-verify**：每个 anchor/vector 改一字节看测试红。
4. **reviewer**：每 phase 末过 `ecc:rust-reviewer`（裁判轴=长远正确，非 ROI）。
5. **档2 frozen-vector 纪律**（ADR-013）：kernel vector 带 provenance（内核版本/日期）+ refresh owner。
6. **共享树**：hunk 隔离提交（`nvme_of_tcp_target` 多会话热点：amber-lynx/silver-bear 在 completion.rs/
   io.rs/mod.rs；先 neighbors 对齐区域）。
7. **同步文档**：改完同步 ROADMAP 矩阵 NVMe-oF 行 + RUNBOOK_HOST_ROOT + crate SPEC_CONFORMANCE。
8. **usnvmemu 无主 CI**：`cd usnvmemu/crates/nvme_of_tcp_target && cargo test`（根目录 `-p` 报包不存在，
   见 memory `usnvmemu-no-ci-gate-workspace-excluded`）；新 standing 测试进 `.github/workflows/usnvmemu.yml`。

---

## 6. 已核验事实锚点

- 三机制文件：`src/tls.rs`(rustls 0.23 ring, server+mTLS) / `tls_identity.rs`(NQN↔cert) /
  `tls_psk.rs`(deterministic, 未接握手, 13 自洽测) / `dhchap.rs`(simplified+spec4msg, compute_response `||`)。
- transcript 隐患：`async_session.rs` Spec4Msg 分支仍调 `compute_response`（简化 `||`，非 §8.13.5.3）。
- 内核：WSL2 `6.6.114.1-microsoft`，含 NVME_TCP/FABRICS，**缺 CONFIG_NVME_AUTH**；target 侧
  `nvme_auth_derive_tls_psk` EXPORT_SYMBOL_GPL **已有**（`RUNBOOK_HOST_ROOT.md:78-86,168-209`）。
- rustls：`0.23.40`，**无 external-PSK 公开 API**（survey 2026-06-06：issue#174 open / PR#2424/#2524
  closed-no-merge）；`Cargo.toml` rustls features=`std,tls12,ring`。
- workspace.exclude：`nvme_of_tcp_target` 在根 `Cargo.toml [workspace.exclude]`，无主 CI 覆盖。
- LESSONS：§2(self-consistent ≠ spec, 含 TLS-PSK HKDF 字段顺序坑) / §12(WebFetch→源验证→anchor) /
  §20(self-consistent 当判据反模式) / §31(重/真机 oracle → frozen-vector gate)。
- 现有文档：`RUNBOOK_HOST_ROOT.md`（§1 dhchap-4+内核出路 / §2 kmod / §3 discovery / §4 disconnect）；
  `docs/plans/2026-06-09-multi-month-architecture-specs.md`(V-spec-strict S1-S4)；
  `crates/.../docs/plans/2026-06-06-phase-v-followup-tls-psk-survey.md`(rustls 调研)。
