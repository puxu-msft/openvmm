# Windows 原生消费 usnvmemu NVMe-oF Target —— 互通可行性手册

> **本文档来历与定位**
> 由姊妹项目 **windows-nvme-controller-emulator**(下称「母项目」)产出,**交给 usnvmemu 团队判断**。
> 母项目原始目标:让 Windows 把一个 software-only 模拟的 NVMe 存储当真设备使用。母项目 spike 实证
> host bare-metal 软件路线不可行(除非 FPGA),guest 形态由 OpenHCL 路径达成。本文档处理**第三条
> Windows 触及路径**:Windows **主机原生**经 inbox NVMe-oF initiator 消费 usnvmemu 的 NVMe-oF TCP target
> —— 不需要 VM、不需要 OpenHCL、不需要任何第三方软件。
>
> **本文档不替 usnvmemu 下结论**。它把母项目在 Windows 存储栈侧的**实证证据**摆出来 + 标注**唯一
> 决定性未决项**,请 usnvmemu 据此判断是否值得投入。所有"确认"项均来自 WS2025 实机 probe(见 §10 附录)。
>
> **置信度图例**:✅ 实机确认 / ❓ 未决(需实验) / ⚠️ 已知 caveat(确认成立) / 📋 推断(待证)

**实证环境**:Windows Server 2025 Standard,Build 26100(`xpu-3.local`),2026-06-13 实机 probe。

---

## 1. TL;DR + 给 usnvmemu 的裁决请求

- ✅ **WS2025 自带 inbox NVMe-oF initiator**(`stornvmeofi.sys` + `nvmeofutil.exe`),开箱即用,无需第三方。
- ❓ **唯一决定性未决**:该 initiator 是否支持 **NVMe/TCP**(usnvmemu 的 target 是 TCP)。driver 二进制证据**偏 RDMA**(KRDMA 主导、无字面 `tcp`);管理面参数却 **transport-无关**。**静态检查无法定论,必须一次真连测试。**
- ⚠️ **架构 caveat**:即便连通,设备呈现为 `BusTypeNvmeof`(0x14)经 storport miniport,**不是** `BusTypeNvme`/stornvme.sys。即"Windows 原生消费 NVMe-oF 块存储",**不是**"Windows 看到 PCIe NVMe controller"。与 OpenHCL guest 路径目标不同(见 §8)。
- ✅ **usnvmemu 已有匹配件**:NVMe-oF TCP target + Discovery + DH-HMAC-CHAP,正好对上 Windows 侧的 SubsystemPort/discovery/`authkey`。

**请 usnvmemu 判断的三个问题**(详见 §9):
1. 是否值得为"Windows 主机原生零-VM 消费"这条路投入一次 host-root 真连测试?
2. 若 Windows inbox 只支持 RDMA(很可能),是否把它当作 ROADMAP **V9 RDMA transport** 的**首个真消费方**(Windows 自带 initiator 作 RDMA 互通靶,免去自找 RDMA initiator)?
3. `BusTypeNvmeof ≠ NVMe controller` 的呈现差异,对 usnvmemu 的目标可接受吗?

---

## 2. 背景:这条路在版图里的位置

母项目穷举了"把 NVMe 呈现给 Windows"的解空间(falsifier 法 + red-team review)。结论收敛到三条**可行**的 software-only 触及方式,usnvmemu 已实现前两条,本文档补第三条:

| # | 路径 | 形态 | 需要 VM? | 需要第三方? | 呈现 | usnvmemu 现状 |
|---|------|------|---------|-----------|------|--------------|
| 1 | OpenHCL VTL2 + PCIe Remote(vsock) | guest 内真 NVMe controller | ✅ Hyper-V | ❌ | `BusTypeNvme`(stornvme.sys) | ✅ 已真机闭环(commit `4fe6bec1`) |
| 2 | OpenHCL VTL2 + vfio-user | guest 内真 NVMe controller | ✅ Hyper-V | ❌ | `BusTypeNvme`(stornvme.sys) | ✅ U1-U5 已 ship |
| 3 | **Windows inbox NVMe-oF initiator(本文)** | **主机原生 NVMe-oF 块设备** | **❌ 零 VM** | **❌ 全 inbox** | `BusTypeNvmeof`(stornvmeofi.sys) | ❓ **从未测过 Windows 消费方** |

第 3 条的独特价值:**唯一不需要 Hyper-V/guest 的 Windows 触及方式**,且 usnvmemu 的 target 代码已 production 级 —— 只差验证 Windows 这端能否连上。

> 母项目曾(2026-05-27)在纯文档调研中**错误**判定"WS2025 无 inbox NVMe-oF initiator"(因 Learn / What's-new 未提)。本文档以 2026-06-13 **实机 probe 证伪了那个旧结论** —— 这正是母项目 research 当时 deferred 却从未执行的那个 `%WINDIR%\inf` grep。教训:负面结论(某能力不存在)只有实机验证才算数。

---

## 3. 实证:WS2025 inbox NVMe-oF initiator 全貌(✅ 确认)

### 3.1 驱动 + 服务

| 项 | 值 | 来源 |
|----|----|----|
| INF | `stornvmeofi.inf` → `%MSFT% = NVMEOF, NTamd64`;`Root\stornvmeofi` | `%WINDIR%\inf` grep |
| 驱动 | `stornvmeofi.sys` **136704 B**,版本 **10.0.26100.32860** | `System32\drivers` |
| 驱动描述 | **"Microsoft NVMeoF Initiator Storport Miniport Driver"**(CompanyName: Microsoft) | VersionInfo |
| 服务 | `stornvmeofi`,**Stopped / Manual / kernel driver**(按需启动) | `Win32_SystemDriver` |
| INF 注释 | `; NOTE: bustype 0x14 is value of BusTypeNvmeof` | INF 原文 |
| 管理 CLI | `nvmeofutil.exe`("Console NVMeoF utility for Windows"),版本 10.0.26100.4484 | `System32` |

> **关键观察**:Microsoft 自己的 NVMe-oF initiator 也是 **storport miniport**(印证母项目 research:storport miniport 是 Windows 存储设备的标准落地方式)。所以呈现为 `BusTypeNvmeof` 经 storport,而非 stornvme.sys 的 PCIe-NVMe 路径。

### 3.2 旁证(同机相关驱动)

- `nvmedisk.sys` v10.0.26100.32995("Nvme Disk Driver",Running/Boot)—— WS2025 NVMe 栈有更新,与 `stornvme.sys` 并存。
- 真实 PCIe NVMe 盘 BusType=`NVMe`(基线确认:本机 KIOXIA NVMe SSD)。
- inbox **无** `nvme.exe`/`nvmecli.exe`(Linux 风格 CLI),管理走 `nvmeofutil.exe`。

---

## 4. 连接模型:`nvmeofutil` 管理面 ↔ usnvmemu target 要暴露什么(✅ 确认管理面)

`nvmeofutil.exe` 子命令:`list / add / remove / connect / disconnect / host / authkey`。

### 4.1 `add SubsystemPort`(定义远端 target 的接入点)
关键参数 —— **直接对应 usnvmemu NVMe-oF target 必须提供的东西**:

| nvmeofutil 参数 | 含义 | usnvmemu target 对应 |
|----------------|------|---------------------|
| `-ta [transport address]` | target 传输地址(IP) | target 监听 IP |
| `-ts [transport service id]` | 传输服务 id(TCP=端口) | target 监听端口(如 4420) |
| `-nq [subsystem nqn]` | subsystem NQN | usnvmemu 的 subsys NQN |
| `-ds {true|false}` | 该 port 是否 **discovery** 功能 | usnvmemu **有 Discovery**(V7) |
| `-dy {true|false}` | 动态 vs 静态 controller model | usnvmemu controller 模型需对齐 |
| `-pi [port id]` / `-hg [host gateway]` | port id / host gateway | initiator 侧拓扑 |
| `-as [max admin queue size]` | admin queue 上限 | usnvmemu admin queue |
| `-cq [default CQT ms]` | 默认 CQT(connect 超时) | keep-alive / 超时对齐 |

### 4.2 `connect`(连到具体 controller)
`-ia 适配器号 / -sp 子系统端口 / -ci controller id`(静态 controller model 用)/ `-qc 最大 IO 队列数 / -qs 最大 IO 队列大小 / -ts 覆盖服务 id`。
- **`-hk [host auth key id]` / `-sk [subsystem auth key id]`** —— **NVMe-oF 认证密钥** = DH-HMAC-CHAP。**usnvmemu 已有完整 CHAP/DH-HMAC 全栈**,正好对上。

### 4.3 `authkey`
专门管理 NVMe-oF 认证密钥(host/subsystem)。usnvmemu 的 V-followup CHAP 工作与此**直接互补**。

> **结论**:Windows 侧管理面是 **transport-无关的标准 NVMe-oF 模型**(SubsystemPort + NQN + discovery + static/dynamic controller + CHAP key)。usnvmemu 的 target 概念上**全部对得上**。唯一缺的是 §5 的传输层是否对齐。

---

## 5. 决定性未决 #1:TCP vs RDMA(❓ 必须实验)

usnvmemu 的 target 是 **NVMe-oF over TCP**。Windows inbox initiator 是否支持 TCP,是这条路成立与否的**单点开关**。

**证据(双向,不一致 → 必须实测)**:

| 证据 | 指向 |
|------|------|
| `stornvmeofi.sys` strings:`onecore\drivers\storage\krdma\*` **大量** KRDMA(kernel RDMA / NDK)符号;`WskRdmaDisconnectEvent` | **偏 RDMA** |
| `stornvmeofi.sys` strings:**无任何字面 `tcp` / `transport` 串** | **偏 RDMA**(TCP 路径若有,通常留 "tcp" 字样) |
| `WskRegister`/`WskCaptureProviderNPI`(Winsock Kernel client 注册) | 中性(NDK 也经 WSK,**不能**当 TCP 证据) |
| `nvmeofutil add/connect` 参数 **transport-无关**(transport address + service id,既适配 TCP 也适配 RDMA),**无** `-transport tcp|rdma` 显式开关 | 中性(不排除 TCP) |

📋 **推断**(待证):WS2025 inbox `stornvmeofi` **很可能 RDMA-only(NVMe/RDMA via NDK/KRDMA)**,因为 driver 二进制无 TCP 痕迹而 RDMA 实现完整。但管理面 transport-无关,**不能据静态分析排除 TCP**。

**一锤定音的测试(usnvmemu 侧最该做的一件事)**:
1. 在 Linux/WSL 起 usnvmemu `nvme_of_tcp_target`(已有),监听 `0.0.0.0:4420`,暴露一个 namespace。
2. WS2025 主机:`sc start stornvmeofi`(或 PnP 安装 `Root\stornvmeofi`)→ `nvmeofutil add -t sp -ta <usnvmemu_ip> -ts 4420 -nq <subsys_nqn> -ds true`(先 discovery)→ `nvmeofutil connect ...`。
3. **观测 oracle**:`nvmeofutil list` 是否出现 controller;`Get-PhysicalDisk` 是否出现 `BusType=NVMeof`(0x14)的盘;能否 `Get-Disk` / 初始化 / 读写。
4. **判据**:
   - 连通 + 出盘 + IO → **TCP 支持成立**,本路打通,usnvmemu 多一个零-VM Windows 消费方。
   - `connect` 报 transport 不支持 / 只接受 RDMA service → **TCP 不支持**,转 §9-问题 2(把 Windows 当 RDMA 互通靶)。

> 母项目可代跑这个测试(有 WS2025 实机 + 网络),但需要 usnvmemu 侧提供一个**可监听的 TCP target 二进制/命令**。这是两个项目的**最小协作接口**。

---

## 6. 架构 caveat:`BusTypeNvmeof` ≠ NVMe controller(⚠️ 确认成立)

即使 §5 测试通过,必须对 usnvmemu 说清楚**这条路达成的不是什么**:

- 设备呈现为 **`BusTypeNvmeof`(0x14)**,经 `stornvmeofi.sys`(storport miniport),设备节点 `Root\stornvmeofi`。
- **不是** `BusTypeNvme`、**不经** stornvme.sys、**不是** PCIe NVMe controller。Device Manager 里是"NVMeoF Initiator Adapter"下的远端盘,不是"NVM Express Controller"。
- 对比:OpenHCL guest 路径(§2 路 1/2)让 guest 的 **stornvme.sys** 绑定,呈现真 `BusTypeNvme` PCIe NVMe controller —— 那才是母项目原始严格目标(D1a)。

**取舍**:本路用"主机原生 + 零 VM + 零第三方"换"呈现为 NVMe-oF 而非 PCIe NVMe"。是否可接受取决于 usnvmemu 的目标定义(消费存储 vs 必须呈现 NVMe controller)。

---

## 7. usnvmemu 已具备的匹配件 + 缺口

✅ **已有,直接复用**:
- NVMe-oF **TCP target**(V1-V8e,production 级)
- **Discovery**(V7)↔ Windows `add SubsystemPort -ds true`
- **DH-HMAC-CHAP / TLS / mTLS**(V-followup)↔ Windows `nvmeofutil authkey` + `connect -hk/-sk`
- 静态/动态 controller model 概念

❓/📋 **需对齐或确认的缺口**:
- **传输层**:若 Windows RDMA-only → usnvmemu 需 **RDMA transport(ROADMAP V9)**;Windows inbox initiator 正好是现成 RDMA 互通靶(见 §9-问题 2)。
- **静态 controller model**:Windows `connect -ci` 暗示支持 static controller model(`add SubsystemPort -dy false` 默认 static)。usnvmemu 需确认 target 在 static model 下行为对齐(Windows 默认 static,Linux nvme-cli 常用 dynamic —— **这是 Linux 测试覆盖不到的 Windows 特有路径**)。
- **CQT / keep-alive 语义**:Windows `-cq [default CQT ms]` 的具体超时行为 vs usnvmemu keep-alive 实现。
- **认证互通**:Windows `authkey` 的 DH-HMAC-CHAP 密钥格式 / 算法集是否与 usnvmemu 对齐(usnvmemu 有 DH-2048..8192;Windows 支持哪些待测)。

> **母项目 research 已记的相邻 caveat**(供 usnvmemu 参考):`BusTypeNvmeof` 在 user-mode `winioctl.h` 也列出但无独立描述,仅 kernel `ntddstor.h` 有完整定义 —— Windows 工具链对 NVMe-oF 盘的识别在 user/kernel 两侧枚举值一致。

---

## 8. 三条 Windows 触及路径横向对比(给 usnvmemu 决策用)

| 维度 | OpenHCL vsock | OpenHCL vfio-user | **Windows inbox NVMe-oF(本文)** |
|------|--------------|-------------------|----------------------------------|
| 需要 Hyper-V / guest | ✅ | ✅ | ❌ **零 VM,主机原生** |
| 需要第三方软件 | ❌ | ❌ | ❌ **全 inbox(WS2025 自带)** |
| 呈现 | `BusTypeNvme`(真 NVMe controller) | `BusTypeNvme` | ⚠️ `BusTypeNvmeof`(NVMe-oF 盘) |
| 走 stornvme.sys | ✅ | ✅ | ❌ 走 stornvmeofi.sys |
| 传输 | VMBus vsock | Unix socket vfio-user | ❓ TCP?/ RDMA(待 §5 测) |
| usnvmemu 复用度 | 高 | 高 | **高(TCP target 现成)** |
| 部署复杂度 | 中(需 build IGVM + 配 VTL2) | 中 | **低(纯网络 + nvmeofutil)** |
| 达成母项目严格 D1a | ✅ | ✅ | ❌(NVMe-oF ≠ PCIe NVMe) |

**一句话**:本路是**部署最轻、最"原生"**的 Windows 消费方式,代价是呈现为 NVMe-oF 而非 PCIe NVMe controller,且 TCP 支持待证。

---

## 9. 给 usnvmemu 的三个裁决问题

1. **要不要打通这条路?**(host-native 零-VM Windows 消费)。若要,最小动作 = §5 一次 host-root 真连测试(母项目可代跑,需 usnvmemu 提供可监听 TCP target 命令)。
2. **若 Windows inbox 仅 RDMA**:是否把它当 ROADMAP **V9 RDMA transport** 的**首个真消费方**?——Windows 自带 `stornvmeofi` 是现成、零成本的 RDMA NVMe-oF 互通靶(soft-RoCE 或真 RDMA NIC),省去自找 RDMA initiator。这可能让 V9 的"真 host 互通"门槛大幅下降。
3. **呈现差异可接受吗?**`BusTypeNvmeof` 经 storport miniport,非 PCIe NVMe controller。若 usnvmemu 目标是"Windows 能用上我的存储",可接受;若必须"Windows 看到 NVMe controller",则仍走 OpenHCL guest 路径(§2 路 1/2)。

---

## 10. 附录:原始 probe 证据 + 复现命令

**复现环境**:WS2025 Build 26100,PowerShell。

```powershell
# A. inbox NVMe-oF initiator 是否存在
Select-String -Path "$env:WINDIR\inf\*.inf" -Pattern "nvmeof|nvmf|fabricsclient|BusTypeNvmeof"
Get-CimInstance Win32_SystemDriver | ? Name -match "nvme|nvmf|fabric" | ft Name,State,StartMode,PathName
(Get-Item "$env:WINDIR\System32\drivers\stornvmeofi.sys").VersionInfo | fl FileDescription,FileVersion

# B. 管理 CLI
nvmeofutil.exe /?
nvmeofutil.exe add -?      # SubsystemPort / NvmeController 参数
nvmeofutil.exe connect -?  # -hk/-sk 认证密钥

# C. TCP vs RDMA 静态线索(不决定性,仅参考)
#   driver strings 含大量 onecore\drivers\storage\krdma\*,无字面 tcp
```

**关键 probe 输出摘录**(2026-06-13 `xpu-3.local`):
- `stornvmeofi.sys` = "Microsoft NVMeoF Initiator **Storport Miniport** Driver" v10.0.26100.32860,服务 `stornvmeofi`(Stopped/Manual,`Root\stornvmeofi`)。
- `nvmeofutil.exe` v10.0.26100.4484,子命令含 `connect` / `authkey`;`add SubsystemPort` 接 `-ta/-ts/-nq/-ds/-dy/-as/-cq`;`connect` 接 `-hk/-sk`(CHAP key)。
- driver strings:`krdma`/`NDK`/`WskRdmaDisconnectEvent` 主导;**无字面 `tcp`/`transport`**。
- INF 注释:`bustype 0x14 is value of BusTypeNvmeof`。

> 完整 probe 脚本见母项目 `spikes/03-openhcl-vpci-nvme/`(`win-nvmeof-probe*.ps1`)。

---

**文档状态**:✅ inbox initiator 存在 = 实机确认;❓ TCP 支持 = 待 usnvmemu 决定是否实测;⚠️ 呈现差异 = 确认成立。**请 usnvmemu 据 §9 裁决。**
