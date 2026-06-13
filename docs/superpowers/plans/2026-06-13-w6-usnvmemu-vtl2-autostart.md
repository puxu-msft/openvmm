# W6 收尾 — usnvmemu VTL2 自启动托管服务 设计/实施计划

> **状态**：设计待 architect review。承重假设①（IGVM bake）**已真机 POC PASS**（见下）。
> 目标 = 让 usnvmemu 在 VTL2 boot **自启动**，替代 operator 手动 `ohcldiag-dev run + base64`
> 推 + `setsid` 起。属 vfio-user-in-underhill（W6b/c + Layer C 已闭环）的 production 收尾。

**Goal**：boot OpenHCL VM（配好 instance）→ guest **自动出 NVMe 盘**，**无需任何 operator 介入**。

**Architecture**：三层，env-gated，未设 env 零行为变化：
1. **二进制**：usnvmemu（static musl ELF）烤进 IGVM initrd 的 `/bin/usnvmemu`。
2. **启动器**：`underhill_init`（VTL2 PID1）env-gated 在 spawn underhill **之前** spawn usnvmemu。
3. **设备**：`underhill_core` 现有 `OPENHCL_VFIO_USER_NVME=<guid>:<sock>` 呈现设备 + 持久 reconnect（不改）。

---

## 已验证 / 承重假设

### ✅ 承重假设①（已真机 POC PASS，2026-06-13）— IGVM bake 可行，零 flowey 改动
`cargo xflowey build-igvm` 已支持 `--custom-extra-rootfs <config>`（`build_igvm.rs:176`，perftoolsfs.config
是先例）。POC：写 `file /bin/usnvmemu <musl> 0755 0 0` 的 config → build → boot → VTL2 内
`ls -l /bin/usnvmemu` = 3148048 root 0755 present；裸跑报自身参数错（`requires --vfio-user-sock`）
= **执行正常**。`update-rootfs.py` 用 gen_init_cpio 格式，OPENHCL_* env 插值。`/bin/sh`(busybox) +
`/tmp`(tmpfs 可写) production 也在。

### ✅ 承重假设②（真机 PASS 2026-06-13）— init spawn 的 usnvmemu 建 socket + 设备连上
init 在 spawn underhill **前** spawn usnvmemu → usnvmemu 监听 unix socket → underhill 起 → device
shim 连上。**timing 非承重**（W6b 持久 reconnect：socket 何时出现都能连，Lost↔Live revive）。
usnvmemu 启动**不依赖** `/dev/mshv_vtl_low`（DMA fd 由 underhill 在 DMA_MAP 时经 SCM_RIGHTS 给，
usnvmemu 只 mmap）→ 只需 `/tmp`（恒在）。**验收**：真机 boot 不手动推任何东西 → guest 出盘 + IO。

### 平台真相（已查，定调设计）— VTL2「非预期进程死亡即 fatal」
`underhill_init` sysctl：`core_pattern=|/bin/underhill-crash`（lib.rs:497）+ `panic_on_oom=1`
（注释 lib.rs:502「Any unexpected process termination is a fatal error anyway, so panic」）。
`reap_until`（lib.rs:284）reaps 所有子进程但仅在 **underhill 主子** 死时返回。
**含义**：crash-restart-loop supervisor **与平台哲学冲突**（usnvmemu segfault → core_pattern →
underhill-crash，可能 fatal）。而 **graceful 停**（SIGTERM，如本项目全程 `pkill` 测试）**不**触发
underhill-crash（信号死 ≠ core dump）—— 这正是 W6b operator 模型可行的原因。
→ **设计定调：本期只做 auto-START（boot 起一次），不做 crash-restart**。usnvmemu 死后的恢复仍
靠 operator/reconnect（与今天一致）。「supervised restart」是另一个有哲学张力的课题，留后续。

---

## 实施（subagent-driven，env-gated 零回归）

### Task 1 — 仓库 rootfs config（bake 落地）
- **Create** `openhcl/usnvmemu_fs.config`（镜像 perftoolsfs.config）：
  ```
  # usnvmemu (vfio-user NVMe server) baked into VTL2 initrd for auto-start.
  file /bin/usnvmemu  ${OPENHCL_USNVMEMU_PATH}  0755 0 0
  ```
  用 env 插值（**不**硬编码路径）：operator 先 cross-build usnvmemu musl，设
  `OPENHCL_USNVMEMU_PATH=<musl bin>`，再 `build-igvm ... --custom-extra-rootfs openhcl/usnvmemu_fs.config`。
- **验证**：承重假设① POC 已证机制；此 task 仅把 POC 的临时 config 固化进仓库 + 文档化。

### Task 2 — `underhill_init` env-gated auto-start usnvmemu（PID1，最敏感）— **architect review 已纳入**
- **Files**: `openhcl/underhill_init/src/lib.rs`（`do_main`，`setup()` 之后、`run()` 之前，约 :576 模块加载段前）。
- **配置（M-1：单一真相源，**不**重复 sock）**：
  - sock **派生自现有** `OPENHCL_VFIO_USER_NVME=<guid>:<sock>[,opts][;...]`（init 取第一条 entry 的
    sock = `split_once(':')` 后 `split(',').next()`；多设备 autostart 留未来）。
  - 新 env `OPENHCL_VFIO_USER_NVME_AUTOSTART=<size_mb>:<backing>`（colon 分隔、**空格-free** 以经 kernel cmdline→init env 不被截断；仅启动器需的 size+backing，设备侧不需）
    = autostart 开关。未设 → **跳过，零行为变化**。
- **逻辑**（env-gated，全程**不得**让 error 逃出 `do_main`）：
  1. **H-1 CVM gate（必做）**：`if is_confidential_vm() { warn+skip }`（`is_confidential_vm()` 在 init
     已用，lib.rs:149——隔离 VM 下 usnvmemu 走 mshv_vtl_low 非隔离共享内存不可用，且省内存约束 VTL2 的空跑进程）。
  2. 从 `OPENHCL_VFIO_USER_NVME` 派生 sock；解析 `AUTOSTART` 的 `<size_mb>:<backing>`（colon，空格-free）。
     **M-2 边界校验**：`size_mb` 非数 / 0 → warn+skip（NvmeController::open 拒 <512B）。backing **须在 `/tmp`**。
  3. 创建 backing file（`File::create(backing)`+`set_len(size_mb<<20)`）—— usnvmemu **不**自建
     （`controller/mod.rs:1813` open 无 `.create`，要文件已在）。
  4. `Command::new("/bin/usnvmemu").args(["--vfio-user-sock", sock, "--backing-file", backing])`，
     **`pre_exec` 闭包内两 syscall（C-1 + H-3）**：
     - **C-1（CRITICAL）`setrlimit(RLIMIT_CORE, {0,0})`** → usnvmemu segfault **不触发 core_pattern**
       （`|/bin/underhill-crash` 无 PID 过滤，会向 host 流 core dump = **假 VTL2 crash 上报**；underhill-crash
       自身也靠 RLIMIT_CORE 防递归）。令异常退出**静默退化为 boot-absent**（reconnect shim 容忍）。
     - **H-3 `setsid()`** → 独立 session，与 init 的 boot session 信号解耦（长存 daemon 卫生）。
     - **M-3** stderr `dup2`→ ttyprintk/kmsg（**非** /tmp 文件；与 init/underhill 日志同流，省 tmpfs RAM）。
     - `.spawn()` **不 wait**（`reap_until` 的 `libc::wait()` 在其死时 reap，pid≠underhill 主子故不返回→无僵尸）。
  - 用 init 现有 spawn 先例（lib.rs:216 sh / :271 openvmm_hcl）；**不**加 restart loop（平台哲学；C-1 后
    未来若做 supervised restart 可仅在 `WIFEXITED(0)`/`SIGTERM` 优雅退出时重启，绝不在异常退出时——门已留好）。
- **H-2 失败 posture（结构性保证）**：autostart 块**绝不**用会逃出 `do_main` 的 `?`（逃出的 `Err`→`main`
  `exit(1)`→**PID1 死→kernel panic**）。用自包含 `if let Err(e) = (|| -> anyhow::Result<()> {...})() { warn }`
  或逐句 `if let Err`。所有失败 → `tracelimit` warn + 继续到 `run()`（退化 boot-absent，device shim 等它）。

> **architect 裁定（已纳入）**：C-1 是真 correctness/safety 洞（silent-degrade 是本设计前提，segfault 路径
> 违反它向 host 报假崩）→ 必修。H-1/H-2/H-3 必做。M-1 派生 sock 灭 mismatch footgun。no-restart 是
> **正确**首切（异常退出须静默退化，平台 crash 机制让任何更响的反应=假崩报告）。承重假设② 仍须真机验。

### Task 3 — 配置打通 + 文档
- 文档化两 env 协同（operator 设 VM command line）：
  - `OPENHCL_VFIO_USER_NVME=<guid>:<sock>`（设备，underhill_core，现有）
  - `OPENHCL_VFIO_USER_NVME_AUTOSTART=<size_mb>:<backing>`（启动器，init，新；sock 自动从设备 env 派生，**不**重复）。
- 更新 `usnvmemu/docs/MILESTONES.md §3.4`（标 auto-start 落地）+ committed harness
  `scripts/hyperv_vfio_user_interop/`（加「不推 usnvmemu，纯靠 autostart」的 e2e 变体）+ ROADMAP。
- **build-igvm 便利 flag**（可选 nicety，非必须）：build_igvm.rs 加 `--with-vfio-user-nvme` 自动
  push `openhcl/usnvmemu_fs.config`（仿 `--with-perf-tools` push perftoolsfs.config，build_igvm.rs:326）。

### ✅ Task 4 — 真机验证（承重假设② PASS）
fresh boot（baked IGVM + 两 env，**不**手动推/起任何东西）→ kmsg `reconnected, Live` →
guest PSDirect 出盘「OpenHCL Userspace NVMe v2.0」+ 4MiB IO markerMatch + oracle-2 raw backing 扫。
对比今天必须 operator setsid——**零 operator 介入**即 PASS。

---

## blast radius / 协调
- `underhill_init`（PID1）：env-gated，未设 `OPENHCL_VFIO_USER_NVME_AUTOSTART` 零变化 → 对所有现有
  OpenHCL 部署零回归。这是最敏感处，architect 重点看。
- `openhcl/usnvmemu_fs.config`（新文件）：纯增。
- `build_igvm.rs`（可选 flag）：纯增 opt-in。
- **不碰 nvme_firmware**（silver-lynx 在改 CMB；usnvmemu 不加 backing-auto-create，由 init 建文件）。
- docs：ROADMAP/MILESTONES 我可随便改（silver-lynx 已确认不碰），用 hunk-based commit 仍稳妥。

## 未来（非本期）
- supervised restart（区分 graceful-exit vs crash，对齐 VTL2 fatal-death 哲学）——有张力，留后续。
- 持久 backing（VTL2 tmpfs backing 重启即失；真 persistent storage 是更大课题）。
- 多设备 / build-igvm 一等 flag。

> **真机 PASS（commit 见下）**：boot 两 env（device + `AUTOSTART=256:/tmp/nvme_backing.img`，皆空格-free 过 cmdline）→
> init 自启 `/bin/usnvmemu`（pid 35，args 由 env 构造）+ 建 256MiB backing → device shim 连上 → guest
> 自动出盘「OpenHCL Userspace NVMe v2.0」256MB + 4MiB IO markerMatch + oracle-2 @22577152。**零 operator**。
> usnvmemu 日志经 dup2(2,1) 入 kmsg（HIGH-1 修证）。
