//! Linux: `/proc`, sysfs and `CLOCK_BOOTTIME`.

use std::path::{Path, PathBuf};

use tokio::time::Instant;

use super::super::{ProviderError, Shared};

const POWER_SUPPLY: &str = "/sys/class/power_supply";

fn parse_kb_field(value: &str) -> Option<u64> {
    value.trim().strip_suffix("kB")?.trim().parse().ok()
}

/// Whole-percent memory usage from `/proc/meminfo`: `(MemTotal - MemAvailable) / MemTotal`.
/// `MemAvailable` (not `MemFree`) accounts for reclaimable cache, matching what `free`/`top`
/// report as "used".
fn parse_meminfo_used_percent(contents: &str) -> Option<u64> {
    let mut total = None;
    let mut avail = None;
    for line in contents.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb_field(v);
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb_field(v);
        }
    }
    match (total, avail) {
        (Some(total), Some(avail)) if total > 0 => Some(total.saturating_sub(avail) * 100 / total),
        _ => None,
    }
}

/// `read_to_string` whose error names the file (a bare "No such file" is useless in a badge).
fn read_sys(path: impl AsRef<Path>) -> Result<String, ProviderError> {
    let path = path.as_ref();
    std::fs::read_to_string(path).map_err(|e| ProviderError::Io(format!("{}: {e}", path.display())))
}

/// Each entry under `root` with its trimmed `type`; an unreadable `root` yields nothing.
fn power_supplies(root: &Path) -> impl Iterator<Item = (PathBuf, String)> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let path = entry.path();
            let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
            (path, kind.trim().to_owned())
        })
}

/// True if any non-battery power supply (Mains, USB, Wireless, …) under `root` reports
/// `online == 1`. No such supply at all (desktop) is treated as AC.
fn on_ac_at(root: &Path) -> bool {
    let mut saw_non_battery = false;
    for (path, kind) in power_supplies(root) {
        if kind == "Battery" {
            continue;
        }
        saw_non_battery = true;
        let online = std::fs::read_to_string(path.join("online")).unwrap_or_default();
        if online.trim() == "1" {
            return true;
        }
    }
    !saw_non_battery
}

pub fn on_ac() -> bool {
    on_ac_at(Path::new(POWER_SUPPLY))
}

/// `<root>/<device>/capacity`, or the first system battery with a readable `capacity`:
/// `Battery` supplies whose `scope` isn't `Device` (peripherals like a wireless mouse), `BAT*`
/// names first, sorted for determinism.
fn sysfs_battery(root: &Path, device: Option<&str>) -> Result<Vec<u8>, ProviderError> {
    if let Some(dev) = device {
        let capacity = read_sys(root.join(dev).join("capacity"))?;
        return Ok(capacity.trim().as_bytes().to_vec());
    }
    let mut candidates: Vec<PathBuf> = power_supplies(root)
        .filter(|(path, kind)| {
            kind == "Battery"
                && std::fs::read_to_string(path.join("scope"))
                    .map_or(true, |s| s.trim() != "Device")
        })
        .map(|(path, _)| path)
        .collect();
    candidates.sort_by_key(|path| {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let name = name.unwrap_or_default();
        (!name.starts_with("BAT"), name)
    });
    for path in candidates {
        if let Ok(capacity) = std::fs::read_to_string(path.join("capacity")) {
            return Ok(capacity.trim().as_bytes().to_vec());
        }
    }
    Err(ProviderError::NoBattery)
}

pub(in crate::provider) async fn battery(
    device: Option<&str>,
    deadline: Instant,
    shared: &Shared,
) -> Result<Vec<u8>, ProviderError> {
    let device = device.map(str::to_owned);
    shared
        .blocking
        .run(deadline, move || {
            sysfs_battery(Path::new(POWER_SUPPLY), device.as_deref())
        })
        .await
        .ok_or(ProviderError::Timeout)?
}

pub(in crate::provider) fn mem_used_percent() -> Result<u64, ProviderError> {
    let contents = read_sys("/proc/meminfo")?;
    parse_meminfo_used_percent(&contents)
        .ok_or_else(|| ProviderError::Io("malformed /proc/meminfo".to_string()))
}

/// Whole seconds since boot, including time suspended — what `/proc/uptime` reports.
pub(in crate::provider) fn uptime_secs() -> Result<u64, ProviderError> {
    // SAFETY: zeroed `timespec` is valid plain data, filled by the call below.
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: `ts` is a live, writable `timespec`.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    u64::try_from(ts.tv_sec).map_err(|e| ProviderError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, body) in files {
            std::fs::write(dir.join(file), body).unwrap();
        }
    }

    fn cap(root: &Path, device: Option<&str>) -> Result<String, ProviderError> {
        sysfs_battery(root, device).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn parse_meminfo_used_percent_computes_from_total_and_available() {
        let meminfo =
            "MemTotal:       10000 kB\nMemFree:         1000 kB\nMemAvailable:    4000 kB\n";
        assert_eq!(parse_meminfo_used_percent(meminfo), Some(60));
        assert_eq!(parse_meminfo_used_percent("garbage"), None);
    }

    #[test]
    fn parse_meminfo_used_percent_rejects_incomplete_input() {
        assert_eq!(parse_meminfo_used_percent("MemTotal: 100 kB\n"), None);
        assert_eq!(
            parse_meminfo_used_percent("MemTotal: 0 kB\nMemAvailable: 0 kB\n"),
            None
        );
        assert_eq!(
            parse_meminfo_used_percent("MemTotal: 100\nMemAvailable: 40 kB\n"),
            None
        );
    }

    #[test]
    fn battery_skips_device_scope_and_prefers_bat() {
        let tmp = TempDir::new("bat-scope");
        let root = tmp.path();
        supply(
            root,
            "hidpp_battery_0",
            &[
                ("type", "Battery\n"),
                ("scope", "Device\n"),
                ("capacity", "11\n"),
            ],
        );
        supply(
            root,
            "BAT1",
            &[
                ("type", "Battery\n"),
                ("scope", "System\n"),
                ("capacity", "77\n"),
            ],
        );
        assert_eq!(cap(root, None).unwrap(), "77");
        assert_eq!(cap(root, Some("hidpp_battery_0")).unwrap(), "11");
    }

    #[test]
    fn battery_skips_entry_without_capacity() {
        let tmp = TempDir::new("bat-nocap");
        let root = tmp.path();
        supply(root, "BAT0", &[("type", "Battery\n")]);
        supply(root, "BAT1", &[("type", "Battery\n"), ("capacity", "42\n")]);
        assert_eq!(cap(root, None).unwrap(), "42");
        assert!(matches!(
            cap(TempDir::new("bat-none").path(), None),
            Err(ProviderError::NoBattery)
        ));
    }

    #[test]
    fn on_ac_follows_mains_online() {
        let tmp = TempDir::new("ac");
        let root = tmp.path();
        supply(root, "BAT0", &[("type", "Battery\n")]);
        supply(root, "AC", &[("type", "Mains\n"), ("online", "0\n")]);
        assert!(!on_ac_at(root));
        supply(root, "AC", &[("online", "1\n")]);
        assert!(on_ac_at(root));
        // No non-battery supply (desktop) counts as AC.
        let bat_only = TempDir::new("ac-none");
        supply(bat_only.path(), "BAT0", &[("type", "Battery\n")]);
        assert!(on_ac_at(bat_only.path()));
    }
}
