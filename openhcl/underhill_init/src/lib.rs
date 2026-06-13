// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements the the Underhill initial process.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
// UNSAFETY: Calling libc functions to set up global system state.
#![expect(unsafe_code)]

mod options;
mod syslog;

// `pub` so that the missing_docs warning fires for options without
// documentation.
pub use options::Options;

use anyhow::Context;
use libc::STDERR_FILENO;
use libc::STDIN_FILENO;
use libc::STDOUT_FILENO;
use libc::c_void;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ffi::OsStr;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::prelude::*;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use syslog::SysLog;
use walkdir::WalkDir;

const UNDERHILL_PATH: &str = "/bin/openvmm_hcl";

struct FilesystemMount<'a> {
    source: &'a CStr,
    target: &'a CStr,
    fstype: &'a CStr,
    options: &'a CStr,
    flags: u64,
}

impl<'a> FilesystemMount<'a> {
    pub fn new(
        source: &'a CStr,
        target: &'a CStr,
        fstype: &'a CStr,
        flags: u64,
        options: &'a CStr,
    ) -> Self {
        Self {
            source,
            target,
            fstype,
            options,
            flags,
        }
    }

    pub fn mount(&self) -> io::Result<()> {
        // SAFETY: calling the API according to the documentation
        let err = unsafe {
            libc::mount(
                self.source.as_ptr(),
                self.target.as_ptr(),
                self.fstype.as_ptr(),
                self.flags,
                self.options.as_ptr().cast::<c_void>(),
            )
        };

        if err != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

mod dev_random_ioctls {
    pub const MAX_ENTROPY_SIZE: usize = 256;
    const RANDOM_IOC_MAGIC: u8 = b'R';
    #[repr(C)]
    pub struct RndAddEntropy {
        pub entropy_count: i32,
        pub buf_size: i32,
        pub buf: [u8; MAX_ENTROPY_SIZE],
    }
    // RNDADDENTROPY _IOW( 'R', 0x03, int [2] )
    nix::ioctl_write_ptr_bad!(
        rnd_add_entropy_ioctl,
        nix::request_code_write!(RANDOM_IOC_MAGIC, 0x3, size_of::<std::os::raw::c_int>() * 2),
        RndAddEntropy
    );
}

// If it is available, use host-generated entropy to speed up boot and
// improve entropy quality.
//
// This is especially useful on machines without hardware random number
// generation. When random numbers are requested from /dev/random, the
// system blocks until it has gained enough entropy.
//
// It is safe to apply host-provided entropy even to a confidential VM,
// because host-provided data is hashed into the existing entropy sources.
// However, we don't know if the entropy from the host can be trusted.
// Therefore we don't want to increase the entropy count in case the kernel
// has not already filled its entropy pool via safe means, so we just write
// to /dev/random instead of using rnd_add_entropy_ioctl.
fn use_host_entropy() -> anyhow::Result<()> {
    use dev_random_ioctls::MAX_ENTROPY_SIZE;

    let host_entropy = match fs_err::read("/proc/device-tree/openhcl/entropy/reg") {
        Ok(contents) => contents,
        Err(e) => {
            log::warn!("Did not get entropy from the host: {e:#}");
            return Ok(());
        }
    };

    if host_entropy.len() > MAX_ENTROPY_SIZE {
        log::warn!(
            "Truncating host-provided entropy (received {} bytes)",
            host_entropy.len()
        );
    }
    let use_entropy_bytes = std::cmp::min(host_entropy.len(), MAX_ENTROPY_SIZE);
    log::info!("Using {} bytes of entropy from the host", use_entropy_bytes);

    let mut entropy = dev_random_ioctls::RndAddEntropy {
        entropy_count: (use_entropy_bytes * 8) as i32,
        buf_size: use_entropy_bytes as i32,
        buf: [0; MAX_ENTROPY_SIZE],
    };
    entropy.buf[..use_entropy_bytes].copy_from_slice(&host_entropy[..use_entropy_bytes]);

    let mut dev_random = fs_err::OpenOptions::new()
        .write(true)
        .open("/dev/random")
        .with_context(|| ("failed to open dev random for setting entropy").to_string())?;

    if underhill_confidentiality::is_confidential_vm() {
        // Just write to /dev/random (and don't increase entropy count)
        dev_random
            .write_all(&entropy.buf[..use_entropy_bytes])
            .context("write to /dev/random")?;
    } else {
        // Write to /dev/random and increase the entropy count
        // so that we can speed up boot when the host entropy can be trusted.
        // SAFETY: API called according to the documentation.
        unsafe {
            dev_random_ioctls::rnd_add_entropy_ioctl(dev_random.as_raw_fd(), &entropy)
                .context("rnd_add_entropy_ioctl")?;
        }
    }

    Ok(())
}

fn setup(
    stat_files: &[&str],
    options: &Options,
    writes: &[(&str, &str)],
    filesystems: &[FilesystemMount<'_>],
) -> anyhow::Result<()> {
    log::info!("Mounting filesystems");

    for filesystem in filesystems {
        let path: &Path = OsStr::from_bytes(filesystem.target.to_bytes()).as_ref();
        // Ensure the target exists.
        fs_err::create_dir_all(path)?;

        filesystem
            .mount()
            .with_context(|| format!("failed to mount {}", path.display()))?;
    }

    log::info!("Command line args: {:?}", options);

    if log::log_enabled!(log::Level::Trace) {
        for stat_file in stat_files {
            if let Ok(file) = fs_err::File::open(stat_file) {
                log::trace!("{}", stat_file);
                for line in BufReader::new(file).lines() {
                    if let Ok(line) = line {
                        log::trace!("{}", line);
                    }
                }
            }
        }
    }

    log::info!("Setting system resource limits and parameters");

    for (path, data) in writes {
        fs_err::write(path, data).with_context(|| format!("failed to write {data}"))?;
    }

    use_host_entropy().context("use host entropy")?;

    Ok(())
}

fn run_setup_scripts(scripts: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    let mut new_env = Vec::new();
    for setup in scripts {
        log::info!("Running provided setup script {}", setup);

        let result = Command::new("/bin/sh")
            .arg("-c")
            .arg(setup)
            .stderr(Stdio::inherit())
            .output()
            .context("script failed to start")?;

        if !result.status.success() {
            anyhow::bail!("setup script failed: {}", result.status);
        }

        // Capture key-value pairs in the script's stdout as environment
        // variables.
        for line in result.stdout.split(|&x| x == b'\n') {
            if let Some((key, value)) = std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.split_once('='))
            {
                log::info!("setting env var {}={}", key, value);
                new_env.push((key.into(), value.into()));
            }
        }
    }
    Ok(new_env)
}

fn run(options: &Options, env: impl IntoIterator<Item = (String, String)>) -> anyhow::Result<()> {
    let mut command = Command::new(UNDERHILL_PATH);
    command.arg("--pid").arg("/run/underhill.pid");
    command.args(&options.underhill_args);
    command.envs(env);

    // Update the file descriptor limit for the main process, since large VMs
    // require lots of fds. There is no downside to a larger value except that
    // we may less effectively catch fd leaks (which have not historically been
    // a problem). So use a value that is plenty large enough for any VM.
    let limit = 0x100000;
    // SAFETY: calling according to docs.
    unsafe {
        if libc::prlimit(
            0,
            libc::RLIMIT_NOFILE,
            &libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            },
            std::ptr::null_mut(),
        ) < 0
        {
            return Err(io::Error::last_os_error()).context("failed to update rlimit");
        }
    }

    log::info!("running {:?}", &command);

    let child = command.spawn().context("underhill failed to start")?;

    let status = reap_until(child).context("wait failed")?;
    if status.success() {
        log::info!("underhill exited successfully");
    } else {
        log::error!("underhill terminated unsuccessfully: {}", status);
    }

    std::process::exit(status.code().unwrap_or(255));
}

/// Reap zombie processes until `child` exits. Return `child`'s exit status.
fn reap_until(child: Child) -> io::Result<ExitStatus> {
    loop {
        let mut status = 0;
        // SAFETY: calling according to docs.
        let pid = unsafe { libc::wait(&mut status) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }

        if pid == child.id() as i32 {
            // The child process died. Pass through the exit status.
            return Ok(ExitStatus::from_raw(status));
        }
    }
}

fn move_stdio(src: impl Into<std::fs::File>, dst: RawFd) {
    assert!((0..=2).contains(&dst));
    let src = src.into();
    if src.as_raw_fd() != dst {
        // SAFETY: calling as documented.
        let r = unsafe { libc::dup2(src.as_raw_fd(), dst) };
        assert_eq!(r, dst);
    } else {
        let _ = src.into_raw_fd();
    }
}

fn init_logging() {
    // Open /dev/null for replacing stdin and stdout.
    move_stdio(fs_err::File::open("/dev/null").unwrap(), STDIN_FILENO);

    move_stdio(
        fs_err::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .unwrap(),
        STDOUT_FILENO,
    );

    // Set stderr to /dev/ttyprintk to catch panic stack.
    let ttyprintk_err = match fs_err::OpenOptions::new()
        .write(true)
        .open("/dev/ttyprintk")
    {
        Ok(ttyprintk) => {
            move_stdio(ttyprintk, STDERR_FILENO);
            None
        }
        Err(err) => Some(err),
    };

    // Set the log output to use /dev/kmsg directly.
    let syslog = SysLog::new().expect("failed to open /dev/kmsg");
    log::set_boxed_logger(Box::new(syslog)).expect("no logger already set");

    // TODO: syslog should respect the OPENVMM_LOG env variable to allow runtime
    // log level changes without rebuilding, but for now downgrade the default
    // to info to stop noisy logs and allow compile time changes for local
    // debugging.
    log::set_max_level(log::LevelFilter::Info);

    // Now that logging is initialized, fail if opening ttyprintk failed.
    // Otherwise, we probably won't see the failure reason in the logs.
    if let Some(err) = ttyprintk_err {
        log::error!("failed to open stderr output: {}", err);
        panic!();
    }
}

fn load_modules(modules_path: &str) -> anyhow::Result<()> {
    // Get the kernel command line.
    let cmdline = fs_err::read_to_string("/proc/cmdline")?;
    let mut params = HashMap::new();
    for option in cmdline.split_ascii_whitespace() {
        if let Some((module, option)) = option.split_once('.') {
            if option.contains('=') {
                let v: &mut String = params.entry(module.replace('-', "_")).or_default();
                *v += option;
                *v += " ";
            }
        }
    }

    // Load the modules.
    for module in WalkDir::new(modules_path).sort_by_file_name() {
        let module = module?;
        if !module.file_type().is_file() {
            continue;
        }

        let module = module.path();
        let module_name = module
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .replace('-', "_");

        let params = params.get_mut(&module_name);

        log::info!(
            "loading kernel module {}: {}",
            module.display(),
            params.as_ref().map_or("", |s| s.as_str())
        );
        let file = fs_err::File::open(module).context("failed to open module")?;

        let params = if let Some(params) = params {
            // Null terminate
            params.pop();
            params.push('\0');
            params.as_bytes()
        } else {
            b"\0"
        };

        // SAFETY: calling the syscall as documented. Of course, the module
        // being loaded has full kernel privileges, but the contents of the file
        // system are trusted.
        let r =
            unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), params.as_ptr(), 0) };
        if r < 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("failed to load module {}", module.display()));
        }

        log::info!("load complete for {}", module.display());
    }

    // Once the kernel modules are loaded into memory, the module files are not needed anymore.
    // By deleting them after, we can save some memory.
    fs_err::remove_dir_all(modules_path)?;

    Ok(())
}

fn timestamp() -> u64 {
    let mut tp;
    // SAFETY: calling `clock_gettime` as documented.
    unsafe {
        tp = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut tp);
    }
    Duration::new(tp.tv_sec as u64, tp.tv_nsec as u32).as_nanos() as u64
}

fn do_main() -> anyhow::Result<()> {
    let boot_time = timestamp();

    init_logging();

    log::info!(
        "Initial process: crate_name={}, crate_revision={}, crate_branch={}",
        env!("CARGO_PKG_NAME"),
        option_env!("BUILD_GIT_SHA").unwrap_or("UNKNOWN_REVISION"),
        option_env!("BUILD_GIT_BRANCH").unwrap_or("UNKNOWN_BRANCH"),
    );

    let stat_files = [
        "/proc/uptime",
        "/proc/timer_list",
        "/proc/interrupts",
        "/proc/meminfo",
        "/proc/iomem",
        "/proc/ioports",
        "/proc/sys/kernel/pid_max",
        "/proc/sys/kernel/threads-max",
        "/proc/sys/vm/max_map_count",
    ];
    let options = Options::parse();
    let writes = &[
        // The kernel sets the maximum number of threads to a number
        // inferred from the size of RAM: the thread structures must
        // occupy only 1/8th of the available RAM pages. That is quite
        // small for Underhill in the interactive mode so the kernel
        // would allow only a small number of threads which doesn't
        // let the interactive mode run.
        ("/proc/sys/kernel/threads-max", "32768"),
        // Censor kernel pointers in the logs for security
        ("/proc/sys/kernel/kptr_restrict", "1"),
        // Enable transparent hugepages on requested VMAs. This is used to map
        // VTL0 memory with huge pages. Although this is on by default in our
        // kernel configuration, the kernel turns it off for low-memory systems
        // (which VTL2 is).
        ("/sys/kernel/mm/transparent_hugepage/enabled", "madvise"),
        // Configure the vmbus devices to be handled as user-mode vmbus
        // driver.
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // GET
            "8dedd1aa-9056-49e4-bfd6-1bf90dc38ef0",
        ),
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // UART
            "8b60ccf6-709f-4c11-90b5-229c959a9e6a",
        ),
        (
            "/sys/bus/vmbus/drivers/uio_hv_generic/new_id",
            // Crashdump
            "427b03e7-4ceb-4286-b5fc-486f4a1dd439",
        ),
        (
            "/proc/sys/kernel/core_pattern",
            if underhill_confidentiality::confidential_filtering_enabled() {
                // Disable the processing of dumps for CVMs.
                ""
            } else {
                // When a user mode crash occurs, the kernel will call `/bin/underhill-crash`
                // passing the information of the crashing process to it.
                // The order of these arguments must match exactly with the order
                // that underhill_crash is expecting.
                "|/bin/underhill-crash %p %i %s %e"
            },
        ),
        // Handle one crashing process at a time.
        ("/proc/sys/kernel/core_pipe_limit", "1"),
        // Don't bother OOM killing processes when out of memory, just panic.
        // Any unexpected process termination is a fatal error anyway, so panic
        // to get a VM crash dump.
        ("/proc/sys/vm/panic_on_oom", "1"),
        // Set the min watermark to 1MiB, the minimum value recommended in
        // Documentation/admin-guide/sysctl/vm.rst (Linux kernel). This controls kswapd.
        // kswapd reclaims memory by swapping or dropping reclaimable caches when the
        // number of free pages in a zone is below the low watermark.
        // VTL2 has no swap and has no reclaimable caches, so there is nothing it can do
        // if it is invoked. By setting the watermarks as low as possible, we
        // ensure that it won't be invoked in normal operation (if it does get invoked, the system
        // is probably about to OOM anyway).
        // This also indirectly controls the size of the percpu pagesets.
        // We want to keep that size as small as possible without introducing contention on the
        // zone lock, as these pages are:
        // * Not counted in MemFree of /proc/meminfo
        // * Not considered when determining if kswapd should be started
        ("/proc/sys/vm/min_free_kbytes", "1024"),
        // Make the high and low watermark as close to the min watermark as possible. This value's
        // units are fractions of 10,000. This means the watermarks will be spaced 0.01% of available
        // memory apart.
        ("/proc/sys/vm/watermark_scale_factor", "1"),
        // Disable the watermark boost feature
        ("/proc/sys/vm/watermark_boost_factor", "0"),
    ];
    let filesystems = [
        FilesystemMount::new(
            c"proc",
            c"/proc",
            c"proc",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"sysfs",
            c"/sys",
            c"sysfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"dev",
            c"/dev",
            c"devtmpfs",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
        FilesystemMount::new(
            c"devpts",
            c"/dev/pts",
            c"devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_RELATIME,
            c"",
        ),
    ];

    setup(&stat_files, &options, writes, &filesystems)?;
    let mut new_env = run_setup_scripts(&options.setup_script)?;
    new_env.push(("KERNEL_BOOT_TIME".into(), boot_time.to_string()));

    if matches!(
        std::env::var("OPENHCL_NVME_VFIO").as_deref(),
        Ok("true" | "1")
    ) {
        // Register VFIO to bind to all NVMe devices, from any vendor.
        //
        // Since nvme is loaded as a module, and that happens after this call,
        // this will take precedence over the in-kernel nvme driver.
        fs_err::write(
            "/sys/bus/pci/drivers/vfio-pci/new_id",
            "ffffffff ffffffff ffffffff ffffffff 010802 ffffff",
        )
        .context("failed to register nvme for vfio")?;
        log::info!("registered vfio-pci as driver for nvme");
    }

    // W6: env-gated auto-start of the baked-in `/bin/usnvmemu` vfio-user NVMe
    // server, so the VTL0 guest gets the emulated NVMe disk with zero operator
    // action. Failures degrade SILENTLY to "boot-absent" (the device shim's
    // persistent reconnect tolerates usnvmemu not running) — and must NEVER
    // escape `do_main` (an `Err` here -> `main` `exit(1)` -> PID1 death ->
    // kernel panic). Hence the explicit swallow-and-warn here.
    if let Err(err) = try_start_vfio_user_nvme() {
        log::warn!("vfio_user_nvme autostart skipped: {err:#}");
    }

    // Start loading modules in parallel.
    let thread = std::thread::spawn(|| {
        if let Err(err) = load_modules("/lib/modules") {
            panic!("failed to load modules: {:#}", err);
        }
    });
    if std::env::var("OPENHCL_WAIT_FOR_MODULES").as_deref() == Ok("1") {
        thread.join().unwrap();
    }

    run(&options, new_env)
}

/// Parse the vfio-user-NVMe auto-start configuration from the two env strings,
/// deriving the listen socket from the **device** env (single source of truth —
/// no socket duplication / mismatch footgun).
///
/// - `device` = `OPENHCL_VFIO_USER_NVME` = `<guid>:<sock>[,opt=...][;<more>]`.
///   The socket is the first entry's `<sock>` (before any `,opts`); multi-device
///   auto-start is future work.
/// - `autostart` = `OPENHCL_VFIO_USER_NVME_AUTOSTART` = `<size_mb>:<backing>`
///   (launcher-only params the device side doesn't need). **Colon-delimited and
///   space-free** so the whole value survives kernel-cmdline → init-env passing
///   (the cmdline splits on whitespace; a space in the value would be truncated).
///   `<backing>` must live under `/tmp` (the only writable tmpfs in VTL2) and
///   contain no `..` (traversal); `<size_mb>` must be 1..=1 TiB (`NvmeController::open`
///   rejects a backing file < 512 bytes, and the upper bound avoids absurd sizes).
///
/// Returns `(sock, backing, size_mb)`.
fn parse_vfio_user_nvme_autostart(
    device: &str,
    autostart: &str,
) -> anyhow::Result<(String, String, u64)> {
    let first = device.split(';').next().unwrap_or("");
    let (_guid, rest) = first
        .split_once(':')
        .context("OPENHCL_VFIO_USER_NVME missing ':' (expected <guid>:<sock>)")?;
    let sock = rest.split(',').next().unwrap_or("").trim();
    anyhow::ensure!(
        !sock.is_empty(),
        "OPENHCL_VFIO_USER_NVME has an empty socket path"
    );

    // `<size_mb>:<backing>` — size first so `split_once(':')` cleanly separates
    // the numeric size (no colon) from the path remainder.
    let (size_str, backing) = autostart
        .split_once(':')
        .context("OPENHCL_VFIO_USER_NVME_AUTOSTART expected <size_mb>:<backing>")?;
    let size_mb: u64 = size_str
        .trim()
        .parse()
        .context("OPENHCL_VFIO_USER_NVME_AUTOSTART <size_mb> is not a number")?;
    let backing = backing.trim();
    anyhow::ensure!(
        (1..=1024 * 1024).contains(&size_mb),
        "OPENHCL_VFIO_USER_NVME_AUTOSTART <size_mb> must be 1..=1048576 (1 TiB); got {size_mb}"
    );
    anyhow::ensure!(
        backing.starts_with("/tmp/") && !backing.contains(".."),
        "OPENHCL_VFIO_USER_NVME_AUTOSTART <backing> must be under /tmp with no '..'; got {backing:?}"
    );

    Ok((sock.to_string(), backing.to_string(), size_mb))
}

/// Env-gated launch of the baked-in `/bin/usnvmemu` vfio-user NVMe server inside
/// VTL2 (see [`parse_vfio_user_nvme_autostart`] for the config).
///
/// Gate: `OPENHCL_VFIO_USER_NVME_AUTOSTART` unset -> no-op, zero behavior change.
///
/// Design (architect-reviewed):
/// - **Confidential-VM gate**: never auto-start on a CVM — usnvmemu DMAs guest
///   RAM via the non-isolated VTL0 shared view (`/dev/mshv_vtl_low`), which is
///   unavailable under isolation. (The device side is also CVM-gated; this is
///   defense-in-depth + avoids a pointless resident process in a CVM.)
/// - **Un-core-dumpable (`RLIMIT_CORE=0`)**: VTL2 sets
///   `core_pattern=|/bin/underhill-crash`, which has *no* PID filter and would
///   stream a core dump to the host = a **false "VTL2 crashed" report** if
///   usnvmemu ever segfaults. With `RLIMIT_CORE=0` an abnormal exit reaps as a
///   plain signal death (no `core_pattern`), degrading silently to boot-absent —
///   the same mechanism `underhill_crash` uses for recursion safety.
/// - **`setsid`**: own session, decoupling usnvmemu's lifecycle signals from
///   PID1's boot session.
/// - **Not waited on**: `reap_until`'s `libc::wait()` reaps it on death (its pid
///   != the underhill child, so the loop doesn't return). **No restart loop** —
///   VTL2 treats unexpected process death as fatal; graceful restart is the
///   operator/reconnect's job.
/// - stdout is **redirected to stderr** (`dup2(2,1)` in `pre_exec`), and init's
///   stderr is `/dev/ttyprintk` -> kmsg. usnvmemu's `tracing` writes to stdout,
///   so this folds its logs into the same kmsg stream operators already watch
///   for "reconnected, Live" — without it they'd hit init's inherited stdout =
///   `/dev/null` and vanish, making the silent-degrade path undiagnosable.
fn try_start_vfio_user_nvme() -> anyhow::Result<()> {
    let Ok(autostart) = std::env::var("OPENHCL_VFIO_USER_NVME_AUTOSTART") else {
        return Ok(()); // gate unset -> no-op
    };

    if underhill_confidentiality::is_confidential_vm() {
        log::warn!("vfio_user_nvme autostart: skipped on confidential VM");
        return Ok(());
    }

    let device = std::env::var("OPENHCL_VFIO_USER_NVME").context(
        "OPENHCL_VFIO_USER_NVME_AUTOSTART set but OPENHCL_VFIO_USER_NVME (device) is unset",
    )?;
    let (sock, backing, size_mb) = parse_vfio_user_nvme_autostart(&device, &autostart)?;

    // usnvmemu's `NvmeController::open` requires the backing file to already
    // exist (it opens without `.create`); create + size it here.
    let f = fs_err::File::create(&backing).context("create vfio_user_nvme backing file")?;
    f.set_len(size_mb << 20)
        .context("size vfio_user_nvme backing file")?;
    drop(f);

    let mut command = Command::new("/bin/usnvmemu");
    command
        .arg("--vfio-user-sock")
        .arg(&sock)
        .arg("--backing-file")
        .arg(&backing)
        .stdin(Stdio::null());
    // SAFETY: `pre_exec` runs in the forked child before `exec`. `setrlimit`
    // and `setsid` are async-signal-safe and touch only this child's state.
    unsafe {
        command.pre_exec(|| {
            // C-1: un-core-dumpable so a segfault never fires
            // core_pattern=|/bin/underhill-crash (false host crash report).
            let rlim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &rlim) < 0 {
                return Err(io::Error::last_os_error());
            }
            // H-3: own session.
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // HIGH-1: route usnvmemu's stdout (where its `tracing` writes) to
            // stderr (= init's /dev/ttyprintk -> kmsg); otherwise it inherits
            // init's stdout = /dev/null and its logs vanish. `dup2` is
            // async-signal-safe.
            if libc::dup2(STDERR_FILENO, STDOUT_FILENO) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.spawn().context("spawn /bin/usnvmemu")?;
    log::info!(
        "vfio_user_nvme autostart: launched /bin/usnvmemu on {sock} (backing {backing}, {size_mb} MiB)"
    );
    Ok(())
}

pub fn main() -> ! {
    match do_main() {
        Ok(_) => unreachable!(),
        Err(err) => {
            log::error!("fatal: {:#}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_vfio_user_nvme_autostart;

    #[test]
    fn derives_sock_and_parses_backing_size() {
        let (sock, backing, size_mb) = parse_vfio_user_nvme_autostart(
            "11111111-2222-3333-4444-555555555555:/tmp/vfio_nvme.sock",
            "256:/tmp/nvme.img",
        )
        .unwrap();
        assert_eq!(sock, "/tmp/vfio_nvme.sock");
        assert_eq!(backing, "/tmp/nvme.img");
        assert_eq!(size_mb, 256);
    }

    #[test]
    fn sock_strips_device_opts_and_extra_entries() {
        // Device env may carry per-device opts (,bar0=..) and multiple entries (;).
        let (sock, ..) = parse_vfio_user_nvme_autostart(
            "g:/tmp/a.sock,bar0=4000,msix=4;h:/tmp/b.sock",
            "64:/tmp/nvme.img",
        )
        .unwrap();
        assert_eq!(sock, "/tmp/a.sock"); // first entry, opts stripped
    }

    #[test]
    fn autostart_value_must_be_space_free_colon_form() {
        // Space-separated would be truncated by the kernel cmdline -> env path,
        // so the parser requires the colon form. A value with no colon fails.
        assert!(parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "/tmp/nvme.img 256").is_err());
        assert!(parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "256").is_err()); // no backing
    }

    #[test]
    fn rejects_backing_outside_tmp_or_traversal() {
        // /tmp is the only writable tmpfs in VTL2; refuse anything else / `..`.
        assert!(parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "256:/var/nvme.img").is_err());
        assert!(
            parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "256:/tmp/../etc/x").is_err(),
            "must reject path traversal"
        );
    }

    #[test]
    fn rejects_bad_size() {
        assert!(parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "0:/tmp/x.img").is_err()); // zero
        assert!(parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "abc:/tmp/x.img").is_err()); // non-numeric
        assert!(
            parse_vfio_user_nvme_autostart("g:/tmp/a.sock", "9999999:/tmp/x.img").is_err(),
            "must reject > 1 TiB"
        );
    }

    #[test]
    fn rejects_malformed_device_env() {
        // No ':' separator -> can't derive sock.
        assert!(parse_vfio_user_nvme_autostart("no-colon-here", "256:/tmp/x.img").is_err());
        // Empty sock.
        assert!(parse_vfio_user_nvme_autostart("g:", "256:/tmp/x.img").is_err());
    }
}
