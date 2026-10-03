//! Non-git builtins: hostname, memory, battery, AC state, uptime, load average.

use super::ProviderError;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("star-forge supports Linux and macOS only");
#[cfg(any(target_os = "macos", test))]
mod pmset;

pub use os::on_ac;
pub(super) use os::{battery, mem_used_percent, uptime_secs};

/// `gethostname(3)` verbatim, i.e. what `hostname` prints: the kernel nodename on Linux;
/// on macOS often the Bonjour/DHCP name with its domain (e.g. `MacBook-Pro.local`). Use a
/// regex `extract` such as `^[^.]+` for the short form.
pub(super) fn hostname() -> Result<String, ProviderError> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a live, writable buffer of the length passed.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
}

/// 1/5/15-minute load averages, two decimals each (same shape as `/proc/loadavg`).
pub(super) fn load_avg() -> Result<String, ProviderError> {
    let mut avg = [0f64; 3];
    // SAFETY: `avg` holds exactly the 3 samples requested.
    if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } != 3 {
        return Err(ProviderError::Io("getloadavg failed".to_string()));
    }
    Ok(format!("{:.2} {:.2} {:.2}", avg[0], avg[1], avg[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_system_builtins_return_values() {
        assert_ne!(hostname().unwrap(), "");
        assert_eq!(load_avg().unwrap().split(' ').count(), 3);
        assert!(uptime_secs().unwrap() > 0);
        assert!(mem_used_percent().unwrap() <= 100);
    }
}
