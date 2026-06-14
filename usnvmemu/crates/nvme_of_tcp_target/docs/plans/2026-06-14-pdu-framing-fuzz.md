# NVMe-oF TCP PDU framing fuzz + hlen-underflow DoS 修复

> 状态：**承重假设已厘清（read_pdu 可经泛型重构 Cursor-驱动）+ 已发现高置信 DoS bug（待 fuzz 跑实证）**。
> 待 architect review → 实现。日期：2026-06-14。归属：`nvme_of_tcp_target`（用户接维护）。
> 来源：fuzz 子项目新战线——第三条 transport 的 wire 解析（parser 金矿，0 fuzz 覆盖）。

## 0. 一句话

给 NVMe-oF TCP 的 **PDU framing 解析**（`framing.rs::read_pdu` 的 PLEN/HLEN/PDO/digest 长度算术，
attacker 控字节）建 coverage-guided fuzz；用方案 A（把 `read_pdu` 泛型化 `<R: Read>`、Cursor 驱动）
fuzz 真 `read_pdu`；顺带修调研中发现的 **hlen<8 整数下溢 DoS**。

## 1. 已发现的 bug（高置信，代码分析；fuzz 跑做独立 oracle 实证）

`framing.rs::read_pdu`(:78) / `read_pdu_async`(:205)：`let psh_len = hlen - CH_LEN;`
- `CommonHdr.hlen: u8`（pdu.rs:75）可为 0；`CH_LEN = 8`（CommonHdr 8 字节）。
- `decode_common_hdr`(pdu.rs:258) 只查 `plen<hlen` / `plen>MAX_PDU_SIZE` / pdu_type 白名单——**漏查
  `hlen ≥ CH_LEN`**。
- 恶意 peer 发 `hlen=0, plen=0, type=ICREQ`：过 `decode_common_hdr`（`0<0` 假、`0>MAX` 假、type 合法）
  → `psh_len = 0usize - 8` **下溢** → `vec![0u8; ~2^64]`(:79/206) → 分配器 abort → **进程 DoS**。
- 类属 [[fuzz-the-contract-not-current-impl]]：合法 peer 恒 `hlen≥8`，现有 306 测试无一发 `hlen<8`。

**修**：`decode_common_hdr` 加 `if (hlen as usize) < CH_LEN → Err(PduError::HlenTooSmall{hlen})`。
**中心化**：sync `read_pdu`(:72) + async `read_pdu_async`(:199) 都经 `decode_common_hdr`，一处修两路全好。
回归测试加在 pdu.rs 既有 `decode_common_hdr` error-path 测试段（pdu.rs:398+）。

## 2. 承重假设（已厘清）

`read_pdu(stream: &mut TcpStream)` 硬绑 `std::net::TcpStream`（`read_exact_or_eof`(:312) 也是）。要
fuzz **真** `read_pdu`（非抄一份——违 "fuzz the contract" 纪律）：

- **方案 A（首选）**：把 `read_pdu` + `read_exact_or_eof` 重构为 **`<R: std::io::Read>` 泛型**。
  `TcpStream: Read`，**现有调用方零改、行为不变**（`WouldBlock/TimedOut` 臂对非-socket R 不触发、保留
  无害）。fuzz 用 `std::io::Cursor<&[u8]>` 喂 fuzzer 字节驱动——快、内存内、无 socket、无 async runtime。
  这是**干净的长期可测性改进**，顺带 enable fuzz。
- 方案 B（弃）：socketpair 真 TcpStream 对——不动生产码但每轮 bind+connect 慢 60×。

> 注：`read_pdu_async<S: AsyncRead>`(:192) 本就泛型，但驱它需 async runtime。sync `read_pdu` 与它是
> **平行实现、共享同一长度算术 + `decode_common_hdr`**——fuzz sync 路径即覆盖同一解析逻辑面；async
> 路径的额外覆盖（tokio 时序）作 follow-up。

## 3. 方案（A）

### 3.1 生产重构（framing.rs，behavior-preserving）
- `read_exact_or_eof<R: std::io::Read>(stream: &mut R, ...)`：body 不变（`stream.read_exact(buf)`，
  `read_exact` 来自 `Read`）。`WouldBlock/TimedOut` 臂保留（非-socket 不触发）。
- `read_pdu<R: std::io::Read>(stream: &mut R) -> anyhow::Result<Pdu>`：签名泛型化，body 不变。
- 调用方（`async_session` 等用 sync `read_pdu` 的）：`TcpStream: Read` 自动满足，零改。
- **验证行为不变**：现有 306 测试全绿（尤其 framing/read_pdu 相关 round-trip）。

### 3.2 bug 修（pdu.rs，§1）
`decode_common_hdr` 加 `hlen ≥ CH_LEN` 校验 + `PduError::HlenTooSmall` variant + 回归测试。

### 3.3 fuzz crate + target（nvme_of_tcp_target/fuzz/，nested workspace，同既有约定）
- `fuzz_read_pdu`：`|data: &[u8]|` → `let mut cur = Cursor::new(data); let _ = read_pdu(&mut cur);`
  （drop 结果——observable oracle 不校验内容）。fuzzer 控**整个 wire 字节流**（CommonHdr + PSH +
  digests + pad + data），压所有长度/偏移算术分支。
- **oracle（observable-only + ASan）**：`read_pdu` 对**任意字节**只能返 `Ok(Pdu)` 或 `Err(PduError)`，
  **绝不 panic / abort(OOM) / hang**。libfuzzer+ASan 兜 abort/OOM；Cursor 短读 → `read_exact`
  `UnexpectedEof` → `PeerClosed` Err（不 hang）。**修前**：hlen<8 → OOM abort（fuzz 立即逮）。
- 限幅：libfuzzer `-max_len`（如 64 KiB ≈ MAX_PDU_SIZE 量级）防超大输入；修后所有 alloc 由
  `hlen≤255` + `plen≤MAX_PDU_SIZE` 界住。

### 3.4 跑 + revert-verify
1. 建 target → **修前跑** → 撞 hlen 下溢 OOM/abort（ASan 报）= **独立 oracle 实证** §1 bug（非仅
   代码分析）+ 可能挖出更多长度算术 bug（plen/pdo/data_len 那串）。
2. 修 `decode_common_hdr` → 再跑 → 该 crash 消失、其余继续探。
3. 回归测试 revert-verify：去掉 hlen 校验 → 测试/fuzz repro 红。

### 3.5 CI
- nightly：本 nightly workflow（`usnvmemu-fuzz-nightly.yml`，我的）的 crate-dir 列表加
  `usnvmemu/crates/nvme_of_tcp_target/fuzz`（第 3 个 crate）；`cargo fuzz list` 自动发现 target。
- stable smoke：`usnvmemu.yml`（共享，有 silver-heron hunk）加一段 build+blind smoke——hunk 隔离提交。
- protoc：nvme_of_tcp_target 经 mesh_protobuf 需 protoc，nightly/smoke 的 restore-packages 已覆盖。

## 4. 执行步骤
1. **重构**（framing.rs read_pdu+read_exact_or_eof 泛型）→ 跑 306 测试验行为不变 → rust-reviewer。
2. **fuzz crate + fuzz_read_pdu** → 修前跑实证 hlen bug。
3. **修** decode_common_hdr（hlen 校验）+ 回归 → re-run fuzz 清 + revert-verify。
4. 处理 fuzz 挖出的其它 finding（逐个验真/artifact，修在 framing/pdu）。
5. CI 接入（nightly crate 列表 + stable smoke hunk）。
6. commit（hunk/pathspec 隔离）；文档同步（本 plan 标完成、ROADMAP/SPEC 若涉及）。

## 5. 待 architect review 确认点
1. **泛型重构 behavior-preserving 吗**：`read_exact_or_eof` 泛型化后 `WouldBlock/TimedOut` 臂对
   `Cursor` 死代码但对 `TcpStream` 保留——有无遗漏的 TcpStream-specific 语义（set_read_timeout/
   partial-resume 的 V6b）被泛型化破坏？
2. **fuzz sync read_pdu 够不够**，还是必须连 async read_pdu_async 一起 fuzz（两者平行、共享算术，但
   async 有 tokio 时序差异）？
3. **hlen 修放 decode_common_hdr** 是否最中心（vs 放 read_pdu）——确认无其它绕过 decode_common_hdr 直
   接算 `hlen-CH_LEN` 的路径。
4. oracle observable-only 够不够，还是该加"Ok(Pdu) 的 plen/hlen 自洽"之类不变量断言。

## 6. architect review 已完成（2026-06-14）—— 裁 WARN/可执行（修订后），泛型重构确认可行

逐条裁定 + 纳入：

- **§5.1 泛型化 → OK**：read_pdu body 无任何 TcpStream-inherent 方法（全 IO 经 read_exact_or_eof，
  其余是 decode_common_hdr/has_*/verify_crc32c/vec!/warn!）；`set_read_timeout` 全在调用方 session.rs、
  不在 body；V6b partial-resume 由 caller + OS TCP buffer 保证，泛型化不触碰。11+ 调用方零改。**纯净可行。**
- **§5.3 hlen 修中心 → OK**：decode_common_hdr 是 sync(:72)+async(:199) 唯一入口，无第三条绕过；一处修
  两路全覆盖（含 read_pdu_async:205 的同源下溢）。
- **§5.4 oracle observable-only → OK（确认保持）**：足够逮 OOM/panic/hang；**不加** "plen/hlen 自洽"
  不变量——那用被测同套假设自证（[[review-not-optional-self-consistent-trap]] 陷阱），且把 fuzz 从
  "找崩溃"偏成"验语义"。保持 observable-only。

- **【BLOCK-1，已纳入】长度算术逐点安全表**：hlen 修**只封 `:78/:205` 一个下溢点**，不是充分。read_pdu
  的每个长度/alloc 点靠**三个独立保护**，缺一仍崩——fuzz 绿才能归因"真无 bug"而非"guard 被悄删"：

  | 点 | 算式 @ | 靠哪个保护 |
  |---|---|---|
  | `psh_len` | hlen-CH_LEN @:78/205 | **本次加的 hlen≥CH_LEN 校验** |
  | `consumed` | hlen+(4?) @:98 | 加法天然不溢（hlen≤255） |
  | `pdo<consumed` reject | @:101（H1 guard） | **已有 :101 guard** |
  | `pad_len` | pdo.saturating_sub(consumed) @:113 | saturating + pdo≤255 |
  | `data_off+ddgst_len` reject | @:123（V8e-1 M-2 guard） | **已有 :123 guard** |
  | `data_len` | plen-data_off-ddgst_len @:130 | **:123 guard** + plen≤MAX_PDU_SIZE |

  → fuzz 修后跑绿 = 这三个保护都在；若 fuzz 又挖出长度 crash = 某 guard 回归（不是新 bug 类）。

- **【WARN-1，死裁定】async 路径纳入范围（不留 open）**：sync/async 是**平行手写实现**，data/pad/digest
  算术是**两份拷贝**（:98-142 vs :226-268），可独立 drift。**承诺**：本轮先 fuzz sync read_pdu（覆盖共享
  算术 + decode_common_hdr）；**紧接 follow-up fuzz read_pdu_async**——其 Cursor 替代 = `futures::io::Cursor`
  或 `tokio_test::io::Builder`（async harness，§3 当前未规划，follow-up 单列工作量）。**不让它消失在"follow-up"
  含糊里**——记入本 plan §7 + ROADMAP。

- **【WARN-2，已纳入】CI 不止改 for 循环**：`usnvmemu-fuzz-nightly.yml` 三处都要加 nvme_of_tcp_target：
  ① for 循环 crate-dir 列表（:78-80）；② `actions/cache` 的 `path:`（:66-68）加 `.../nvme_of_tcp_target/
  fuzz/corpus`（否则新 target corpus 不跨夜累积）；③ `upload-artifact` 的 `path:`（:111-113）加
  `.../nvme_of_tcp_target/fuzz/artifacts`。

- **【WARN-3，文档准确性】** §3.3 措辞改：`-max_len` 控 fuzzer **输入字节数/速度**，**非** read_pdu 内部
  alloc 上界；alloc 上界**独立**由 `plen ≤ MAX_PDU_SIZE`(1 MiB) + hlen 修保证（fuzzer 12 字节输入即可声明
  plen=1MiB 触发 1MiB alloc，可控、非 bug）。

## 7. follow-up（本轮后，已承诺非含糊）
- **async read_pdu_async fuzz**（WARN-1）：futures/tokio-test async Cursor harness，覆盖 async 侧那份
  独立长度算术拷贝。
- fuzz 若挖出 framing 外的 finding（dispatch/reassembler/dhchap）→ 逐个评估新 target。
