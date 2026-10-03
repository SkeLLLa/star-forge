//! macOS: `pmset`, `sysctl` and mach calls.

use std::process::Stdio;
use std::time::{Duration, SystemTime};

use tokio::time::Instant;

use super::super::exec::{RunOpts, run_with_timeout};
use super::super::{ProviderError, Shared};
use super::pmset::{VmPages, pmset_battery_percent, pmset_on_ac, vm_used_percent};

/// Absolute path, so a daemon inheriting an odd `$PATH` still finds it.
const PMSET: &str = "/usr/bin/pmset";
const PMSET_ARGS: [&str; 2] = ["-g", "batt"];
/// `pmset -g batt` prints well under 1 KiB; this only bounds a misbehaving binary.
const PMSET_CAP: usize = 64 * 1024;
/// Bound for `on_ac`'s synchronous probe, inside the daemon's 500 ms maintenance budget:
/// a hung `pmset` must not pin a blocking-pool slot.
const ON_AC_PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Runs `pmset -g batt` synchronously, killing it after `timeout`. Output (far below a
/// pipe buffer) is read only after exit, so the child never blocks on a full pipe.
fn pmset_batt_blocking(timeout: Duration) -> Option<String> {
    use std::io::Read as _;

    let mut child = std::process::Command::new(PMSET)
        .args(PMSET_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if start.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// True unless `pmset -g batt` reports a non-AC source; a failed probe counts as AC.
pub fn on_ac() -> bool {
    pmset_batt_blocking(ON_AC_PROBE_TIMEOUT).is_none_or(|out| pmset_on_ac(&out))
}

/// Whole seconds since `kern.boottime`, including time asleep — what `uptime(1)` reports.
/// (`CLOCK_MONOTONIC` also counts sleep but has an unspecified origin; `CLOCK_UPTIME_RAW`
/// stops while asleep.)
pub(in crate::provider) fn uptime_secs() -> Result<u64, ProviderError> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = size_of::<libc::timeval>();
    // SAFETY: `mib` names 2 levels; `boot`/`len` describe a live, writable `timeval`; no
    // new value is set.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut boot).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let boot = u64::try_from(boot.tv_sec).map_err(|e| ProviderError::Io(e.to_string()))?;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| ProviderError::Io(e.to_string()))?
        .as_secs();
    Ok(now.saturating_sub(boot))
}

/// The host port, fetched once: every `mach_host_self` call adds a send-right reference.
fn mach_host() -> libc::mach_port_t {
    // Declared here: libc's binding is deprecated in favour of the `mach2` crate.
    unsafe extern "C" {
        fn mach_host_self() -> libc::mach_port_t;
    }
    // SAFETY: `mach_host_self` has no preconditions. (A closure, not the fn item:
    // `extern "C"` fns don't implement `FnOnce`.)
    static HOST: std::sync::LazyLock<libc::mach_port_t> =
        std::sync::LazyLock::new(|| unsafe { mach_host_self() });
    *HOST
}

pub(in crate::provider) fn mem_used_percent() -> Result<u64, ProviderError> {
    // Not bound by libc.
    unsafe extern "C" {
        fn host_page_size(
            host: libc::host_t,
            out_page_size: *mut libc::vm_size_t,
        ) -> libc::kern_return_t;
    }
    let mut total: u64 = 0;
    let mut len = size_of::<u64>();
    // SAFETY: the name is NUL-terminated; `total`/`len` describe a live, writable `u64`;
    // no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut total).cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `vm_statistics64` is plain integer data; all-zero is a valid value.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` is a live, writable `vm_statistics64` spanning `count` `integer_t`s.
    let kr = unsafe {
        libc::host_statistics64(
            mach_host(),
            libc::HOST_VM_INFO64,
            (&raw mut stats).cast(),
            &raw mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(ProviderError::Io(format!("host_statistics64 failed: {kr}")));
    }
    // `host_statistics64` counts kernel pages. Under Rosetta, `sysconf(_SC_PAGESIZE)`
    // reports 4 KiB while arm64 kernel pages are 16 KiB, which would read ~4x low;
    // `host_page_size` returns the kernel size (what `vm_stat` uses).
    let mut page_size: libc::vm_size_t = 0;
    // SAFETY: `page_size` is a live, writable `vm_size_t`.
    let kr = unsafe { host_page_size(mach_host(), &raw mut page_size) };
    if kr != libc::KERN_SUCCESS {
        return Err(ProviderError::Io(format!("host_page_size failed: {kr}")));
    }
    let page_size = u64::try_from(page_size).map_err(|e| ProviderError::Io(e.to_string()))?;
    let pages = VmPages {
        internal: u64::from(stats.internal_page_count),
        purgeable: u64::from(stats.purgeable_count),
        wired: u64::from(stats.wire_count),
        compressed: u64::from(stats.compressor_page_count),
    };
    vm_used_percent(pages, page_size, total)
        .ok_or_else(|| ProviderError::Io("hw.memsize is 0".to_string()))
}

/// No sysfs on macOS: parse `pmset -g batt`, run like any command so the badge deadline
/// kills its process group.
pub(in crate::provider) async fn battery(
    device: Option<&str>,
    deadline: Instant,
    shared: &Shared,
) -> Result<Vec<u8>, ProviderError> {
    let out = run_with_timeout(
        PMSET,
        &PMSET_ARGS.map(String::from),
        None,
        deadline,
        PMSET_CAP,
        RunOpts {
            envs: &[],
            unset: &[],
            shared,
        },
    )
    .await?;
    pmset_battery_percent(&String::from_utf8_lossy(&out), device)
        .map(String::into_bytes)
        .ok_or(ProviderError::NoBattery)
}
