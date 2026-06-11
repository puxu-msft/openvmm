// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CLI argument parsing for the underhill core process.

#![warn(missing_docs)]

use anyhow::Context;
use anyhow::bail;
use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::MeshPayload;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Clone, Debug, MeshPayload)]
pub enum TestScenarioConfig {
    SaveFail,
    RestoreStuck,
    SaveStuck,

    /// Exercises a mocked TDISP flow for emulated TDISP devices produced by OpenVMM tests.
    VpciTdispFlow,
}

impl FromStr for TestScenarioConfig {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<TestScenarioConfig, anyhow::Error> {
        match s {
            "SERVICING_SAVE_FAIL" => Ok(TestScenarioConfig::SaveFail),
            "SERVICING_RESTORE_STUCK" => Ok(TestScenarioConfig::RestoreStuck),
            "SERVICING_SAVE_STUCK" => Ok(TestScenarioConfig::SaveStuck),
            "TDISP_VPCI_FLOW_TEST" => Ok(TestScenarioConfig::VpciTdispFlow),
            _ => Err(anyhow::anyhow!("Invalid test config: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload)]
pub enum GuestStateLifetimeCli {
    Default,
    ReprovisionOnFailure,
    Reprovision,
    Ephemeral,
}

impl FromStr for GuestStateLifetimeCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<GuestStateLifetimeCli, anyhow::Error> {
        match s {
            "DEFAULT" | "0" => Ok(GuestStateLifetimeCli::Default),
            "REPROVISION_ON_FAILURE" | "1" => Ok(GuestStateLifetimeCli::ReprovisionOnFailure),
            "REPROVISION" | "2" => Ok(GuestStateLifetimeCli::Reprovision),
            "EPHEMERAL" | "3" => Ok(GuestStateLifetimeCli::Ephemeral),
            _ => Err(anyhow::anyhow!("Invalid lifetime: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload)]
pub enum GuestStateEncryptionPolicyCli {
    Auto,
    None,
    GspById,
    GspKey,
}

impl FromStr for GuestStateEncryptionPolicyCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<GuestStateEncryptionPolicyCli, anyhow::Error> {
        match s {
            "AUTO" | "0" => Ok(GuestStateEncryptionPolicyCli::Auto),
            "NONE" | "1" => Ok(GuestStateEncryptionPolicyCli::None),
            "GSP_BY_ID" | "2" => Ok(GuestStateEncryptionPolicyCli::GspById),
            "GSP_KEY" | "3" => Ok(GuestStateEncryptionPolicyCli::GspKey),
            _ => Err(anyhow::anyhow!("Invalid encryption policy: {}", s)),
        }
    }
}

#[derive(Clone, Copy, Debug, MeshPayload)]
pub enum EfiDiagnosticsLogLevelCli {
    Default,
    Info,
    Full,
}

impl FromStr for EfiDiagnosticsLogLevelCli {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<EfiDiagnosticsLogLevelCli, anyhow::Error> {
        match s {
            "DEFAULT" | "0" => Ok(EfiDiagnosticsLogLevelCli::Default),
            "INFO" | "1" => Ok(EfiDiagnosticsLogLevelCli::Info),
            "FULL" | "2" => Ok(EfiDiagnosticsLogLevelCli::Full),
            _ => Err(anyhow::anyhow!("Invalid EFI diagnostics log level: {}", s)),
        }
    }
}

#[derive(Clone, Debug, MeshPayload, Inspect, InspectMut)]
pub enum KeepAliveConfig {
    EnabledHostAndPrivatePoolPresent,
    DisabledHostAndPrivatePoolPresent,
    Disabled,
}

impl FromStr for KeepAliveConfig {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<KeepAliveConfig, anyhow::Error> {
        match s.to_lowercase().as_str() {
            "host,privatepool" | "enabled" => Ok(KeepAliveConfig::EnabledHostAndPrivatePoolPresent),
            "nohost,privatepool" => Ok(KeepAliveConfig::DisabledHostAndPrivatePoolPresent),
            "nohost,noprivatepool" => Ok(KeepAliveConfig::Disabled),
            x if x == "disabled" || x.starts_with("disabled,") => Ok(KeepAliveConfig::Disabled),
            _ => Err(anyhow::anyhow!("Invalid keepalive config: {}", s)),
        }
    }
}

impl KeepAliveConfig {
    pub fn is_enabled(&self) -> bool {
        matches!(self, KeepAliveConfig::EnabledHostAndPrivatePoolPresent)
    }

    /// Returns the string representation matching the inspect rename attributes.
    pub fn as_str(&self) -> &'static str {
        match self {
            KeepAliveConfig::EnabledHostAndPrivatePoolPresent => "enabled",
            KeepAliveConfig::DisabledHostAndPrivatePoolPresent => "nohost,privatepool",
            KeepAliveConfig::Disabled => "disabled",
        }
    }
}

// We've made our own parser here instead of using something like clap in order
// to save on compiled file size. We don't need all the features a crate can provide.
/// underhill core command-line and environment variable options.
pub struct Options {
    /// (OPENHCL_WAIT_FOR_START=1 | --wait-for-start)
    ///  wait for a diagnostics start request before initializing and starting the VM
    pub wait_for_start: bool,

    /// (OPENHCL_SIGNAL_VTL0_STARTED=1)
    /// immediately signal that VTL0 has started, before doing any
    /// initialization. This allows VM boot to proceed even if initialization
    /// may hang (e.g., because you specified OPENHCL_WAIT_FOR_START=1).
    pub signal_vtl0_started: bool,

    /// (OPENHCL_REFORMAT_VMGS=1 | --reformat-vmgs)
    /// reformat the VMGS file on boot. useful for running potentially destructive VMGS tests.
    pub reformat_vmgs: bool,

    /// (OPENHCL_PID_FILE_PATH=/path/to/file | --pid /path/to/file)
    /// write the PID to the specified path
    pub pid: Option<PathBuf>,

    /// (OPENHCL_VMBUS_MAX_VERSION=\<number\>)
    /// limit the maximum protocol version allowed by vmbus; used for testing purposes
    pub vmbus_max_version: Option<u32>,

    /// (OPENHCL_VMBUS_ENABLE_MNF=1)
    /// Enable handling of MNF in the Underhill vmbus server, instead of the host.
    pub vmbus_enable_mnf: Option<bool>,

    /// (OPENHCL_VMBUS_FORCE_CONFIDENTIAL_EXTERNAL_MEMORY=1)
    /// Force the use of confidential external memory for all non-relay vmbus channels. For testing
    /// purposes only.
    ///
    /// N.B.: Not all vmbus devices support this feature, so enabling it may cause failures.
    pub vmbus_force_confidential_external_memory: bool,

    /// (OPENHCL_VMBUS_CHANNEL_UNSTICK_DELAY_MS=\<number\>) (default: 100)
    /// Delay before unsticking a vmbus channel after it has been opened, in milliseconds. Set to
    /// zero to disable unsticking.
    pub vmbus_channel_unstick_delay_ms: u64,

    /// (OPENHCL_CMDLINE_APPEND=\<string\>)
    /// Command line to append to VTL0, only used with direct boot.
    pub cmdline_append: Option<String>,

    /// (OPENHCL_VNC_PORT=\<number\> | --vnc-port \<number\>) (default: 3)
    /// VNC (vsock) port number
    pub vnc_port: u32,

    /// (OPENHCL_GDBSTUB=1)
    /// Enables the GDB stub for debugging the guest.
    pub gdbstub: bool,

    /// (OPENHCL_GDBSTUB_PORT=\<number\>) (default: 4)
    /// GDB stub (vsock) port number.
    pub gdbstub_port: u32,

    /// (OPENHCL_VTL0_STARTS_PAUSED=1)
    /// Start with VTL0 paused
    pub vtl0_starts_paused: bool,

    /// (OPENHCL_FRAMEBUFFER_GPA_BASE=\<number\>)
    /// Base GPA of the fixed framebuffer mapping for underhill to read.
    /// If a value is provided, a graphics device is exposed.
    // TODO: send this value as an IGVM device tree parameter instead
    pub framebuffer_gpa_base: Option<u64>,

    /// (OPENHCL_SERIAL_WAIT_FOR_RTS=\<bool\>)
    /// Whether the emulated 16550 waits for guest DTR+RTS before pulling data
    /// from the host.
    pub serial_wait_for_rts: bool,

    /// (OPENHCL_FORCE_LOAD_VTL0_IMAGE=\<string\>)
    /// Force load the specified image in VTL0. The image must support the
    /// option specified.
    ///
    /// Valid options are "pcat, uefi, linux".
    pub force_load_vtl0_image: Option<String>,

    /// (OPENHCL_NVME_VFIO=1)
    /// Use the user-mode VFIO NVMe driver instead of the Linux driver.
    pub nvme_vfio: bool,

    /// (OPENHCL_HIDE_ISOLATION=1)
    /// Hide the isolation mode from the guest.
    pub hide_isolation: bool,

    /// (OPENHCL_HALT_ON_GUEST_HALT=1) When receiving a halt request from a
    /// lower VTL, halt underhill instead of forwarding the halt request to the
    /// host. This allows for debugging state without the partition state
    /// changing from the host.
    pub halt_on_guest_halt: bool,

    /// (OPENHCL_NO_SIDECAR_HOTPLUG=1) Leave sidecar VPs remote even if they
    /// hit exits.
    pub no_sidecar_hotplug: bool,

    /// (OPENHCL_NVME_KEEP_ALIVE=\<KeepaliveConfig\>)
    /// Configure NVMe keep alive behavior when servicing.
    /// Options are:
    ///  - "host,privatepool" - Enable keep alive if both host and private pool support it.
    ///  - "nohost,privatepool" - Used when the host does not support keepalive, but a private pool is present. Keepalive is disabled.
    ///  - "nohost,noprivatepool" - Keepalive is disabled.
    ///  - "disabled, X, X" - Keepalive is disabled due to manual
    ///    override. Host and private pool options are ignored.
    pub nvme_keep_alive: KeepAliveConfig,

    /// (OPENHCL_MANA_KEEP_ALIVE=\<KeepAliveConfig\>)
    /// Configure MANA keep alive behavior when servicing.
    /// Options are:
    ///  - "host,privatepool" - Enable keep alive if both host and private pool support it.
    ///  - "nohost,privatepool" - Used when the host does not support keepalive, but a private pool is present. Keepalive is disabled.
    ///  - "nohost,noprivatepool" - Keepalive is disabled.
    ///  - "disabled, X, X" - TODO: This needs to be implemented for mana.
    pub mana_keep_alive: KeepAliveConfig,

    /// (OPENHCL_NVME_ALWAYS_FLR=1)
    /// Always use the FLR (Function Level Reset) path for NVMe devices,
    /// even if we would otherwise attempt to use VFIO's NoReset support.
    pub nvme_always_flr: bool,

    /// (OPENHCL_TEST_CONFIG=\<TestScenarioConfig\>)
    /// Test configurations are designed to replicate specific behaviors and
    /// conditions in order to simulate various test scenarios.
    pub test_configuration: Option<TestScenarioConfig>,

    /// (OPENHCL_DISABLE_UEFI_FRONTPAGE=1) Disable the frontpage in UEFI which
    /// will result in UEFI terminating, shutting down the guest instead of
    /// showing the frontpage.
    pub disable_uefi_frontpage: Option<bool>,

    /// (HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT=1) Instruct UEFI to always attempt a
    /// default boot, even if existing boot entries fail.
    pub default_boot_always_attempt: Option<bool>,

    /// (HCL_GUEST_STATE_LIFETIME=\<GuestStateLifetimeCli\>)
    /// Specify which guest state lifetime to use.
    pub guest_state_lifetime: Option<GuestStateLifetimeCli>,

    /// (HCL_GUEST_STATE_ENCRYPTION_POLICY=\<GuestStateEncryptionPolicyCli\>)
    /// Specify which guest state encryption policy to use.
    pub guest_state_encryption_policy: Option<GuestStateEncryptionPolicyCli>,

    /// (HCL_EFI_DIAGNOSTICS_LOG_LEVEL=\<EfiDiagnosticsLogLevelCli\>)
    /// Specify the EFI diagnostics log level filter (DEFAULT, INFO, or FULL).
    /// Overrides the value in DPS when set.
    pub efi_diagnostics_log_level: Option<EfiDiagnosticsLogLevelCli>,

    /// (HCL_STRICT_ENCRYPTION_POLICY=1) Strict guest state encryption policy.
    pub strict_encryption_policy: Option<bool>,

    /// (HCL_ATTEMPT_AK_CERT_CALLBACK=1) Attempt to renew the AK cert.
    /// If not specified, use the configuration in DPSv2 ManagementVtlFeatures.
    pub attempt_ak_cert_callback: Option<bool>,

    /// (OPENHCL_ENABLE_VPCI_RELAY=1) Enable the VPCI relay.
    pub enable_vpci_relay: Option<bool>,

    /// (OPENHCL_DISABLE_PROXY_REDIRECT=1) Disable proxy interrupt redirection.
    pub disable_proxy_redirect: bool,

    /// (OPENHCL_DISABLE_LOWER_VTL_TIMER_VIRT=1) Disable lower VTL timer virtualization.
    pub disable_lower_vtl_timer_virt: bool,

    /// (OPENHCL_CONFIG_TIMEOUT_IN_SECONDS=\<number\>) (default: 5)
    /// Timeout in seconds for VM configuration operations, both initial
    /// configuration and subsequent modifications.
    pub config_timeout_in_seconds: u64,

    /// (OPENHCL_SERVICING_TIMEOUT_DUMP_COLLECTION_IN_MS=\<number\>) (default: 500)
    /// The default time to wait in milliseconds for dump collection during a
    /// panic in servicing.
    pub servicing_timeout_dump_collection_in_ms: u64,

    /// (OPENHCL_PCIE_REMOTE_INSTANCE=<guid>:<vsock_port>[,handshake_timeout_ms=N];...)
    /// 可重复（用 ';' 分隔多个）。每项注入一个 pcie_remote 实验设备实例。
    /// 仅在 IGVM cmdline policy = APPEND_CHOSEN 下生效；CVM 下被静默过滤。
    /// 端口黑名单（1/2/3/4/0x1337 等）会被拒绝。
    /// spec §3.2 / §3.3。
    pub pcie_remote_instance: Vec<PcieRemoteCliConfig>,

    /// (OPENHCL_PCIE_REMOTE_TAKEOVER=<nvme_guid>:<vsock_port>[,handshake_timeout_ms=N];...)
    /// Path C：把 vmwp 下发的某个 NVMe controller GUID 改派为 pcie_remote。
    /// 该 GUID 必须是用户先通过 `Add-VMNvmeController` 注册的占位 controller，
    /// 这样 vmwp 才会 vpci OFFER。CVM 下被静默过滤。spec §3.1 path C。
    pub pcie_remote_takeover: Vec<PcieRemoteCliConfig>,

    /// (OPENHCL_VFIO_USER_NVME=<guid>:<unix_path>[;...])
    /// 可重复（用 ';' 分隔多个）。每项注入一个 vfio-user NVMe 实验设备实例（W6b）。
    /// 主动连接到指定 AF_UNIX 套接字上的 vfio-user server。CVM 下被静默过滤。
    pub vfio_user_nvme: Vec<VfioUserNvmeCliConfig>,
}

/// 单条 `--pcie-remote-instance` / `OPENHCL_PCIE_REMOTE_INSTANCE` 配置。
#[derive(Clone, Debug, MeshPayload, Inspect)]
pub struct PcieRemoteCliConfig {
    /// 实例 GUID（也作为 vpci bus_instance_id 使用；必须 vmwp 已知）。
    #[inspect(display)]
    pub instance_id: guid::Guid,
    /// vsock 端口（不允许撞 well-known: 1/2/3/4/0x1337/vnc/gdbstub）。
    pub vsock_port: u32,
    /// 握手超时（毫秒，默认 2000）。
    pub handshake_timeout_ms: u32,
}

impl FromStr for PcieRemoteCliConfig {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, anyhow::Error> {
        let mut parts = s.split(',');
        let head = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty config"))?;
        let (guid_s, port_s) = head
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected <guid>:<port>"))?;
        let instance_id: guid::Guid = guid_s
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid guid {guid_s}: {e}"))?;
        let vsock_port: u32 = port_s
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid port {port_s}: {e}"))?;
        let mut handshake_timeout_ms = 2000u32;
        for kv in parts {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("expected key=value: {kv}"))?;
            match k {
                "handshake_timeout_ms" => {
                    handshake_timeout_ms = v
                        .parse()
                        .map_err(|e| anyhow::anyhow!("invalid timeout {v}: {e}"))?;
                }
                _ => anyhow::bail!("unknown key: {k}"),
            }
        }
        Ok(Self {
            instance_id,
            vsock_port,
            handshake_timeout_ms,
        })
    }
}

/// 单条 `OPENHCL_VFIO_USER_NVME` 配置（W6b）。
/// 格式 `<guid>:<unix_path>`——主动连接到该 AF_UNIX 套接字上的 vfio-user server。
/// 与 pcie_remote 不同：用 socket 路径（String）而非 vsock_port（u32），
/// 没有 takeover 路径，也没有 per-entry kv 选项（套接字路径无额外参数）。
#[derive(Clone, Debug, MeshPayload, Inspect)]
pub struct VfioUserNvmeCliConfig {
    /// 实例 GUID（也作为 vpci bus_instance_id 使用；必须 vmwp 已知）。
    #[inspect(display)]
    pub instance_id: guid::Guid,
    /// vfio-user server 监听的 AF_UNIX 套接字路径。
    pub unix_path: String,
    /// 握手超时（毫秒，默认 5000）。
    pub handshake_timeout_ms: u32,
}

impl FromStr for VfioUserNvmeCliConfig {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, anyhow::Error> {
        // 只在第一个 ':' 处切分：guid 在前，路径在后（路径可含其他字符，
        // 但通常不含 ':'）。无 ':' 则报错。
        let (guid_s, path_s) = s
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected <guid>:<unix_path>"))?;
        let instance_id: guid::Guid = guid_s
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid guid {guid_s}: {e}"))?;
        if path_s.is_empty() {
            anyhow::bail!("empty unix_path");
        }
        Ok(Self {
            instance_id,
            unix_path: path_s.to_string(),
            handshake_timeout_ms: 5000,
        })
    }
}

/// 解析 `OPENHCL_VFIO_USER_NVME` 这种 ';' 分隔多项 + 内部 `<guid>:<unix_path>`
/// 的环境变量，产出合法配置列表。非法项 `eprintln!` + `tracing::warn!` 后跳过，
/// **不** boot fail（mirror parse_pcie_remote_entries）。
/// 无端口黑名单校验——socket 路径不与 TCP 端口冲突，跳过 check_vsock_port 类比。
fn parse_vfio_user_nvme_entries(
    raw: &str,
    env_name: &str,
) -> anyhow::Result<Vec<VfioUserNvmeCliConfig>> {
    let mut out = Vec::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        match entry.parse::<VfioUserNvmeCliConfig>() {
            Ok(cfg) => out.push(cfg),
            Err(e) => {
                eprintln!("vfio_user_nvme: {env_name} entry {entry:?} rejected ({e:#}); skipping.");
                tracing::warn!(CVM_ALLOWED, error = %e, env = env_name, "vfio_user_nvme: skip invalid entry");
                continue;
            }
        }
    }
    Ok(out)
}

/// 解析 `OPENHCL_PCIE_REMOTE_{INSTANCE,TAKEOVER}` 这种 ';' 分隔多项 + 内部
/// `<guid>:<port>[,handshake_timeout_ms=N]` 的环境变量，端口黑名单校验后产出
/// 合法配置列表。非法项 `eprintln!` 后跳过，**不** boot fail（spec §3.3）。
fn parse_pcie_remote_entries(
    raw: &str,
    env_name: &str,
    vnc_port: Option<u32>,
    gdbstub_port: Option<u32>,
    config_timeout_secs: u64,
) -> anyhow::Result<Vec<PcieRemoteCliConfig>> {
    // K-19: handshake_timeout_ms 必须 ≤ config_timeout/2，否则 OpenHCL boot
    // 整体被这一个 instance 卡死风险过大。
    let max_handshake_timeout_ms = (config_timeout_secs * 500) as u32; // /2 (ms 单位)
    let mut out = Vec::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let cfg: PcieRemoteCliConfig = entry
            .parse()
            .with_context(|| format!("invalid {env_name} entry: {entry}"))?;
        if cfg.handshake_timeout_ms > max_handshake_timeout_ms {
            eprintln!(
                "pcie_remote: {env_name} entry {} rejected (handshake_timeout_ms={} > max {}=config_timeout/2); skipping.",
                cfg.instance_id, cfg.handshake_timeout_ms, max_handshake_timeout_ms
            );
            tracing::warn!(
                env = env_name,
                handshake_timeout_ms = cfg.handshake_timeout_ms,
                max = max_handshake_timeout_ms,
                "pcie_remote: skip excessive handshake_timeout"
            );
            continue;
        }
        if let Err(e) = pcie_remote_device::transport::check_vsock_port(
            cfg.vsock_port,
            Some(vnc_port.unwrap_or(3)),
            Some(gdbstub_port.unwrap_or(4)),
        ) {
            eprintln!(
                "pcie_remote: {env_name} entry {} rejected ({e}); skipping.",
                cfg.instance_id
            );
            tracing::warn!(CVM_ALLOWED, error = %e, env = env_name, "pcie_remote: skip blacklisted port");
            continue;
        }
        out.push(cfg);
    }
    Ok(out)
}

impl Options {
    pub(crate) fn parse(
        extra_args: Vec<String>,
        extra_env: Vec<(String, Option<String>)>,
    ) -> anyhow::Result<Self> {
        // Pull the entire environment into a BTreeMap for manipulation through extra_env.
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in extra_env {
            match value {
                Some(value) => env.insert(key.into(), value.into()),
                None => env.remove::<OsStr>(key.as_ref()),
            };
        }

        // Reads an environment variable, falling back to a legacy variable (replacing
        // "OPENHCL_" with "UNDERHILL_") if the original is not set.
        let read_legacy_openhcl_env = |name: &str| -> Option<&OsString> {
            env.get::<OsStr>(name.as_ref()).or_else(|| {
                env.get::<OsStr>(
                    format!(
                        "UNDERHILL_{}",
                        name.strip_prefix("OPENHCL_").unwrap_or(name)
                    )
                    .as_ref(),
                )
            })
        };

        // Reads an environment variable strings.
        let read_env = |name: &str| -> Option<&OsString> { env.get::<OsStr>(name.as_ref()) };

        fn parse_bool_opt(value: Option<&OsString>) -> anyhow::Result<Option<bool>> {
            value
                .map(|v| {
                    if v.eq_ignore_ascii_case("true") || v == "1" {
                        Ok(true)
                    } else if v.eq_ignore_ascii_case("false") || v == "0" {
                        Ok(false)
                    } else {
                        Err(anyhow::anyhow!(
                            "invalid boolean environment variable: {}",
                            v.to_string_lossy()
                        ))
                    }
                })
                .transpose()
        }

        fn parse_bool(value: Option<&OsString>) -> bool {
            parse_bool_opt(value).ok().flatten().unwrap_or_default()
        }

        let parse_legacy_env_bool = |name| parse_bool(read_legacy_openhcl_env(name));
        let parse_env_bool = |name: &str| parse_bool(read_env(name));
        let parse_env_bool_opt = |name: &str| {
            parse_bool_opt(read_env(name))
                .map_err(|e| tracing::warn!("failed to parse {name}: {e:#}"))
                .ok()
                .flatten()
        };

        fn parse_number(value: Option<&OsString>) -> anyhow::Result<Option<u64>> {
            value
                .map(|v| {
                    let v = v.to_string_lossy();
                    v.parse()
                        .context(format!("invalid numeric environment variable: {v}"))
                })
                .transpose()
        }

        let parse_legacy_env_number = |name| {
            parse_number(read_legacy_openhcl_env(name))
                .context(format!("parsing legacy env number: {name}"))
        };
        let parse_env_number = |name: &str| {
            parse_number(read_env(name)).context(format!("parsing env number: {name}"))
        };

        let mut wait_for_start = parse_legacy_env_bool("OPENHCL_WAIT_FOR_START");
        let mut reformat_vmgs = parse_legacy_env_bool("OPENHCL_REFORMAT_VMGS");
        let mut pid = read_legacy_openhcl_env("OPENHCL_PID_FILE_PATH")
            .map(|x| x.to_string_lossy().into_owned().into());
        let vmbus_max_version = read_legacy_openhcl_env("OPENHCL_VMBUS_MAX_VERSION")
            .map(|x| {
                vmbus_core::parse_vmbus_version(&(x.to_string_lossy()))
                    .map_err(|x| anyhow::anyhow!("Error parsing vmbus max version: {}", x))
            })
            .transpose()?;
        let vmbus_enable_mnf =
            read_legacy_openhcl_env("OPENHCL_VMBUS_ENABLE_MNF").map(|v| parse_bool(Some(v)));
        let vmbus_force_confidential_external_memory =
            parse_env_bool("OPENHCL_VMBUS_FORCE_CONFIDENTIAL_EXTERNAL_MEMORY");
        let vmbus_channel_unstick_delay_ms =
            parse_legacy_env_number("OPENHCL_VMBUS_CHANNEL_UNSTICK_DELAY_MS")?;
        let cmdline_append = read_legacy_openhcl_env("OPENHCL_CMDLINE_APPEND")
            .map(|x| x.to_string_lossy().into_owned());
        let force_load_vtl0_image = read_legacy_openhcl_env("OPENHCL_FORCE_LOAD_VTL0_IMAGE")
            .map(|x| x.to_string_lossy().into_owned());
        let mut vnc_port = parse_legacy_env_number("OPENHCL_VNC_PORT")?.map(|x| x as u32);
        let framebuffer_gpa_base = parse_legacy_env_number("OPENHCL_FRAMEBUFFER_GPA_BASE")?;
        let vtl0_starts_paused = parse_legacy_env_bool("OPENHCL_VTL0_STARTS_PAUSED");
        let serial_wait_for_rts = parse_legacy_env_bool("OPENHCL_SERIAL_WAIT_FOR_RTS");
        let nvme_vfio = parse_legacy_env_bool("OPENHCL_NVME_VFIO");
        let hide_isolation = parse_env_bool("OPENHCL_HIDE_ISOLATION");
        let halt_on_guest_halt = parse_legacy_env_bool("OPENHCL_HALT_ON_GUEST_HALT");
        let no_sidecar_hotplug = parse_legacy_env_bool("OPENHCL_NO_SIDECAR_HOTPLUG");
        let gdbstub = parse_legacy_env_bool("OPENHCL_GDBSTUB");
        let gdbstub_port = parse_legacy_env_number("OPENHCL_GDBSTUB_PORT")?.map(|x| x as u32);
        let nvme_keep_alive = read_env("OPENHCL_NVME_KEEP_ALIVE")
                    .map(|x| {
                        let s = x.to_string_lossy();
                        match s.parse::<KeepAliveConfig>() {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "failed to parse OPENHCL_NVME_KEEP_ALIVE ('{s}'): {e}. Nvme keepalive will be disabled."
                                );
                                KeepAliveConfig::Disabled
                            }
                        }
                    })
                    .unwrap_or(KeepAliveConfig::Disabled);
        let mana_keep_alive = read_env("OPENHCL_MANA_KEEP_ALIVE")
                    .map(|x| {
                        let s = x.to_string_lossy();
                        match s.parse::<KeepAliveConfig>() {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "failed to parse OPENHCL_MANA_KEEP_ALIVE ('{s}'): {e}. Mana keepalive will be disabled."
                                );
                                KeepAliveConfig::Disabled
                            }
                        }
                    })
                    .unwrap_or(KeepAliveConfig::Disabled);
        let nvme_always_flr = parse_env_bool("OPENHCL_NVME_ALWAYS_FLR");
        let test_configuration = read_env("OPENHCL_TEST_CONFIG").and_then(|x| {
            x.to_string_lossy()
                .parse::<TestScenarioConfig>()
                .map_err(|e| {
                    tracing::warn!(
                        "failed to parse OPENHCL_TEST_CONFIG: {}. No test will be simulated.",
                        e
                    )
                })
                .ok()
        });
        let disable_uefi_frontpage = parse_env_bool_opt("OPENHCL_DISABLE_UEFI_FRONTPAGE");
        let signal_vtl0_started = parse_env_bool("OPENHCL_SIGNAL_VTL0_STARTED");
        let default_boot_always_attempt = parse_env_bool_opt("HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT");
        let guest_state_lifetime = read_env("HCL_GUEST_STATE_LIFETIME").and_then(|x| {
            x.to_string_lossy()
                .parse::<GuestStateLifetimeCli>()
                .map_err(|e| tracing::warn!("failed to parse HCL_GUEST_STATE_LIFETIME: {:#}", e))
                .ok()
        });
        let guest_state_encryption_policy =
            read_env("HCL_GUEST_STATE_ENCRYPTION_POLICY").and_then(|x| {
                x.to_string_lossy()
                    .parse::<GuestStateEncryptionPolicyCli>()
                    .map_err(|e| {
                        tracing::warn!("failed to parse HCL_GUEST_STATE_ENCRYPTION_POLICY: {:#}", e)
                    })
                    .ok()
            });
        let efi_diagnostics_log_level = read_env("HCL_EFI_DIAGNOSTICS_LOG_LEVEL").and_then(|x| {
            x.to_string_lossy()
                .parse::<EfiDiagnosticsLogLevelCli>()
                .map_err(|e| {
                    tracing::warn!("failed to parse HCL_EFI_DIAGNOSTICS_LOG_LEVEL: {:#}", e)
                })
                .ok()
        });
        let strict_encryption_policy = parse_env_bool_opt("HCL_STRICT_ENCRYPTION_POLICY");
        let attempt_ak_cert_callback = parse_env_bool_opt("HCL_ATTEMPT_AK_CERT_CALLBACK");
        let enable_vpci_relay = parse_env_bool_opt("OPENHCL_ENABLE_VPCI_RELAY");
        let disable_proxy_redirect = parse_env_bool("OPENHCL_DISABLE_PROXY_REDIRECT");
        let disable_lower_vtl_timer_virt = parse_env_bool("OPENHCL_DISABLE_LOWER_VTL_TIMER_VIRT");
        let config_timeout_in_seconds =
            parse_legacy_env_number("OPENHCL_CONFIG_TIMEOUT_IN_SECONDS")?.unwrap_or(5);
        let servicing_timeout_dump_collection_in_ms =
            parse_env_number("OPENHCL_SERVICING_TIMEOUT_DUMP_COLLECTION_IN_MS")?.unwrap_or(500);

        let pcie_remote_instance: Vec<PcieRemoteCliConfig> = parse_pcie_remote_entries(
            read_legacy_openhcl_env("OPENHCL_PCIE_REMOTE_INSTANCE")
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            "OPENHCL_PCIE_REMOTE_INSTANCE",
            vnc_port,
            gdbstub_port,
            config_timeout_in_seconds,
        )?;
        let pcie_remote_takeover: Vec<PcieRemoteCliConfig> = parse_pcie_remote_entries(
            read_legacy_openhcl_env("OPENHCL_PCIE_REMOTE_TAKEOVER")
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            "OPENHCL_PCIE_REMOTE_TAKEOVER",
            vnc_port,
            gdbstub_port,
            config_timeout_in_seconds,
        )?;

        let vfio_user_nvme: Vec<VfioUserNvmeCliConfig> = parse_vfio_user_nvme_entries(
            read_legacy_openhcl_env("OPENHCL_VFIO_USER_NVME")
                .and_then(|s| s.to_str())
                .unwrap_or(""),
            "OPENHCL_VFIO_USER_NVME",
        )?;

        let mut args = std::env::args().chain(extra_args);
        // Skip our own filename.
        args.next();

        while let Some(next) = args.next() {
            let arg = next;

            match &*arg {
                "--wait-for-start" => wait_for_start = true,
                "--reformat-vmgs" => reformat_vmgs = true,

                x if x.starts_with("--") && x.len() > 2 => {
                    if let Some(eq) = arg.find('=') {
                        let (name, value) = arg.split_at(eq);
                        // Don't forget to exclude the '=' itself.
                        let value = &value[1..];
                        Self::parse_value_arg(name, value, &mut pid, &mut vnc_port)?;
                    } else {
                        if let Some(value) = args.next() {
                            Self::parse_value_arg(&arg, &value, &mut pid, &mut vnc_port)?;
                        } else {
                            bail!("Expected a value after argument {}", arg);
                        }
                    }
                }
                x => bail!("Unrecognized argument {}", x),
            }
        }

        Ok(Self {
            wait_for_start,
            signal_vtl0_started,
            reformat_vmgs,
            pid,
            vmbus_max_version,
            vmbus_enable_mnf,
            vmbus_force_confidential_external_memory,
            vmbus_channel_unstick_delay_ms: vmbus_channel_unstick_delay_ms.unwrap_or(100),
            cmdline_append,
            vnc_port: vnc_port.unwrap_or(3),
            framebuffer_gpa_base,
            gdbstub,
            gdbstub_port: gdbstub_port.unwrap_or(4),
            vtl0_starts_paused,
            serial_wait_for_rts,
            force_load_vtl0_image,
            nvme_vfio,
            hide_isolation,
            halt_on_guest_halt,
            no_sidecar_hotplug,
            nvme_keep_alive,
            mana_keep_alive,
            nvme_always_flr,
            test_configuration,
            disable_uefi_frontpage,
            default_boot_always_attempt,
            guest_state_lifetime,
            guest_state_encryption_policy,
            efi_diagnostics_log_level,
            strict_encryption_policy,
            attempt_ak_cert_callback,
            enable_vpci_relay,
            disable_proxy_redirect,
            disable_lower_vtl_timer_virt,
            config_timeout_in_seconds,
            servicing_timeout_dump_collection_in_ms,
            pcie_remote_instance,
            pcie_remote_takeover,
            vfio_user_nvme,
        })
    }

    fn parse_value_arg(
        name: &str,
        value: &str,
        pid: &mut Option<PathBuf>,
        vnc_port: &mut Option<u32>,
    ) -> anyhow::Result<()> {
        match name {
            "--pid" => *pid = Some(value.into()),
            "--vnc-port" => {
                *vnc_port = Some(
                    value
                        .parse()
                        .context(format!("Error parsing VNC port {}", value))?,
                )
            }
            x => bail!("Unrecognized argument {}", x),
        }

        Ok(())
    }
}

#[cfg(test)]
mod pcie_remote_tests {
    use super::*;

    #[test]
    fn parse_basic_entry() {
        let s = "deadbeef-0000-0000-0000-000000000000:50000";
        let cfg: PcieRemoteCliConfig = s.parse().unwrap();
        assert_eq!(cfg.instance_id.data1, 0xdead_beef);
        assert_eq!(cfg.vsock_port, 50000);
        assert_eq!(cfg.handshake_timeout_ms, 2000); // default
    }

    #[test]
    fn parse_with_timeout() {
        let s = "deadbeef-0000-0000-0000-000000000000:50000,handshake_timeout_ms=5000";
        let cfg: PcieRemoteCliConfig = s.parse().unwrap();
        assert_eq!(cfg.handshake_timeout_ms, 5000);
    }

    #[test]
    fn parse_rejects_bad_guid() {
        let s = "not-a-guid:50000";
        assert!(s.parse::<PcieRemoteCliConfig>().is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        let s = "deadbeef-0000-0000-0000-000000000000:50000,bogus=x";
        assert!(s.parse::<PcieRemoteCliConfig>().is_err());
    }

    #[test]
    fn parse_entries_skips_blacklisted_ports() {
        // 黑名单端口（1=VSOCK_CONTROL）被静默跳过，不让 boot fail。
        let raw =
            "deadbeef-0000-0000-0000-000000000000:1;feedface-0000-0000-0000-000000000000:50000";
        let out = parse_pcie_remote_entries(raw, "TEST", Some(3), Some(4), 5).unwrap();
        assert_eq!(out.len(), 1, "blacklisted port should be silently skipped");
        assert_eq!(out[0].vsock_port, 50000);
    }

    #[test]
    fn parse_entries_skips_vnc_collision() {
        let raw = "deadbeef-0000-0000-0000-000000000000:5900";
        let out = parse_pcie_remote_entries(raw, "TEST", Some(5900), Some(4), 5).unwrap();
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn parse_entries_handles_empty_string() {
        let out = parse_pcie_remote_entries("", "TEST", None, None, 5).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn parse_entries_invalid_syntax_propagates() {
        // 非法 GUID 整段 raw 解析失败（and_then 不吞下 parse error）
        let raw = "totally:bogus";
        let r = parse_pcie_remote_entries(raw, "TEST", None, None, 5);
        assert!(r.is_err());
    }

    /// K-19: handshake_timeout_ms 超过 config_timeout/2 应被跳过。
    #[test]
    fn parse_entries_rejects_oversized_handshake_timeout() {
        // config_timeout = 5s → max = 2500ms。10000ms 超出。
        let raw = "deadbeef-0000-0000-0000-000000000000:50000,handshake_timeout_ms=10000";
        let out = parse_pcie_remote_entries(raw, "TEST", None, None, 5).unwrap();
        assert!(
            out.is_empty(),
            "oversized handshake_timeout should be skipped"
        );
    }

    /// K-19: handshake_timeout_ms 在限内应被保留。
    #[test]
    fn parse_entries_accepts_in_range_handshake_timeout() {
        // config_timeout = 10s → max = 5000ms。3000ms 在限内。
        let raw = "deadbeef-0000-0000-0000-000000000000:50000,handshake_timeout_ms=3000";
        let out = parse_pcie_remote_entries(raw, "TEST", None, None, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].handshake_timeout_ms, 3000);
    }
}
