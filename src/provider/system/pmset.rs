//! Pure parsers for macOS memory and `pmset` output (compiled on Linux for tests only).

/// The `host_statistics64(HOST_VM_INFO64)` page counts that make up "used" memory on macOS.
#[derive(Debug, Clone, Copy)]
pub(super) struct VmPages {
    pub(super) internal: u64,
    pub(super) purgeable: u64,
    pub(super) wired: u64,
    pub(super) compressed: u64,
}

/// Whole-percent memory usage on macOS, matching Activity Monitor's "Memory Used": app
/// memory (`internal - purgeable`) + wired + compressor-occupied pages, over `hw.memsize`.
/// Like Linux's `MemTotal - MemAvailable`, file cache and purgeable pages count as free.
pub(super) fn vm_used_percent(pages: VmPages, page_size: u64, total_bytes: u64) -> Option<u64> {
    if total_bytes == 0 {
        return None;
    }
    let used_pages = pages
        .internal
        .saturating_sub(pages.purgeable)
        .saturating_add(pages.wired)
        .saturating_add(pages.compressed);
    let used = used_pages.saturating_mul(page_size).min(total_bytes);
    Some(used.saturating_mul(100) / total_bytes)
}

/// `pmset -g batt`'s header (`Now drawing from 'AC Power'`) names the current source; any
/// other (`'Battery Power'`, `'UPS Power'`) is off AC. No header counts as AC, like Linux's
/// no-supply case.
pub(super) fn pmset_on_ac(out: &str) -> bool {
    out.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Now drawing from '")?
                .split_once('\'')
        })
        .is_none_or(|(source, _)| source == "AC Power")
}

/// Charge percent from `pmset -g batt` source lines, e.g.
/// ` -InternalBattery-0 (id=4653155)\t95%; discharging; 4:12 remaining present: true`:
/// the source named exactly `device` (as printed, e.g. `InternalBattery-0` or a UPS name),
/// else the first `InternalBattery*` source.
pub(super) fn pmset_battery_percent(out: &str, device: Option<&str>) -> Option<String> {
    out.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix('-')?;
        let name = rest
            .find(" (id=")
            .or_else(|| rest.find('\t'))
            .map_or(rest, |i| &rest[..i])
            .trim();
        let wanted = device.map_or_else(|| name.starts_with("InternalBattery"), |d| name == d);
        if !wanted {
            return None;
        }
        rest.split(|c: char| c.is_whitespace() || c == ';')
            .filter_map(|tok| tok.strip_suffix('%'))
            .find(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_used_percent_counts_app_wired_and_compressed_not_purgeable() {
        let pages = VmPages {
            internal: 500,
            purgeable: 100,
            wired: 150,
            compressed: 50,
        };
        // (500 - 100 + 150 + 50) pages * 4 KiB over 4000 KiB.
        assert_eq!(vm_used_percent(pages, 4096, 4_096_000), Some(60));
        // Never above 100, and a zero total is an error, not a division by zero.
        assert_eq!(vm_used_percent(pages, 4096, 4096), Some(100));
        assert_eq!(vm_used_percent(pages, 4096, 0), None);
    }

    const PMSET_ON_BATTERY: &str = "Now drawing from 'Battery Power'\n \
        -InternalBattery-0 (id=4653155)\t85%; discharging; 4:12 remaining present: true\n";
    const PMSET_AC_WITH_UPS: &str = "Now drawing from 'AC Power'\n \
        -Back-UPS ES 700 (id=1234)\t100%; charged; 0:00 remaining present: true\n \
        -InternalBattery-0 (id=4653155)\t7%; charging; (no estimate) present: true\n";

    #[test]
    fn pmset_on_ac_reads_the_drawing_from_header() {
        assert!(!pmset_on_ac(PMSET_ON_BATTERY));
        assert!(pmset_on_ac(PMSET_AC_WITH_UPS));
        assert!(!pmset_on_ac("Now drawing from 'UPS Power'\n"));
        // Unparseable output is treated as AC, like a desktop with no supplies.
        assert!(pmset_on_ac(""));
    }

    #[test]
    fn pmset_battery_percent_picks_internal_battery_or_named_device() {
        assert_eq!(
            pmset_battery_percent(PMSET_ON_BATTERY, None),
            Some("85".to_string())
        );
        // Default skips a UPS listed first; `device` selects by the printed name.
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, None),
            Some("7".to_string())
        );
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, Some("Back-UPS ES 700")),
            Some("100".to_string())
        );
        assert_eq!(
            pmset_battery_percent(PMSET_AC_WITH_UPS, Some("InternalBattery-1")),
            None
        );
        // Desktop Mac: header only.
        assert_eq!(
            pmset_battery_percent("Now drawing from 'AC Power'\n", None),
            None
        );
    }
}
