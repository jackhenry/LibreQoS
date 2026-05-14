//! Memory pressure watchdog for controlled `lqosd` restarts.
//!
//! The watchdog samples `/proc` and exits the daemon before the kernel OOM
//! path has to choose a victim. `lqosd.service` is configured with
//! `Restart=always`, so this releases process memory while preserving logs that
//! explain why the restart happened.

use crate::stats::{BUS_REQUESTS, FLOWS_TRACKED, HIGH_WATERMARK, TIME_TO_POLL_HOSTS};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing::{error, info, warn};

const CHECK_INTERVAL_SECONDS: u64 = 15;
const PRESSURE_WARNING_AVAILABLE_BYTES: u64 = mib(3_072);
const DEFAULT_MIN_AVAILABLE_BYTES: u64 = mib(2_304);
const DEFAULT_MIN_PROCESS_BYTES: u64 = mib(1_024);
const DEFAULT_MAX_PROCESS_BYTES: u64 = gib(4);
const DEFAULT_MAX_SWAP_BYTES: u64 = gib(1);
const WATCHDOG_EXIT_CODE: i32 = 75;

/// Spawns the memory watchdog thread.
///
/// Side effects: this function starts a background thread. The thread reads
/// `/proc/meminfo`, `/proc/self/status`, and exits the process when memory
/// pressure reaches the configured critical threshold.
pub fn start_memory_watchdog() {
    if env_flag_enabled("LQOSD_MEMORY_WATCHDOG_DISABLED") {
        warn!("lqosd memory watchdog disabled by LQOSD_MEMORY_WATCHDOG_DISABLED");
        return;
    }

    match std::thread::Builder::new()
        .name("Memory Watchdog".to_string())
        .spawn(memory_watchdog_loop)
    {
        Ok(_) => info!("lqosd memory watchdog started"),
        Err(err) => warn!("Failed to start lqosd memory watchdog: {err:?}"),
    }
}

fn memory_watchdog_loop() {
    let config = MemoryWatchdogConfig::from_env();
    let mut warned_about_pressure = false;

    loop {
        std::thread::sleep(Duration::from_secs(CHECK_INTERVAL_SECONDS));

        let Ok(snapshot) = MemorySnapshot::read() else {
            warn!("Unable to sample lqosd memory state from /proc");
            continue;
        };

        if let Some(reason) = config.critical_reason(&snapshot) {
            log_critical_memory_snapshot(&snapshot, &config, &reason);
            std::thread::sleep(Duration::from_millis(250));
            std::process::exit(WATCHDOG_EXIT_CODE);
        }

        if snapshot.mem_available_bytes < PRESSURE_WARNING_AVAILABLE_BYTES {
            if !warned_about_pressure {
                warn!(
                    "lqosd memory pressure warning: mem_available={} process_rss={} process_swap={} process_total={} threads={}",
                    format_bytes(snapshot.mem_available_bytes),
                    format_bytes(snapshot.process_rss_bytes),
                    format_bytes(snapshot.process_swap_bytes),
                    format_bytes(snapshot.process_total_bytes()),
                    snapshot.thread_count,
                );
                warned_about_pressure = true;
            }
        } else {
            warned_about_pressure = false;
        }
    }
}

fn log_critical_memory_snapshot(
    snapshot: &MemorySnapshot,
    config: &MemoryWatchdogConfig,
    reason: &str,
) {
    error!(
        "lqosd memory watchdog restarting daemon: reason={} mem_available={} mem_total={} process_rss={} process_swap={} process_total={} threads={} thresholds=min_available:{} min_process:{} max_process:{} max_swap:{}",
        reason,
        format_bytes(snapshot.mem_available_bytes),
        format_bytes(snapshot.mem_total_bytes),
        format_bytes(snapshot.process_rss_bytes),
        format_bytes(snapshot.process_swap_bytes),
        format_bytes(snapshot.process_total_bytes()),
        snapshot.thread_count,
        format_bytes(config.min_available_bytes),
        format_bytes(config.min_process_bytes),
        format_bytes(config.max_process_bytes),
        format_bytes(config.max_swap_bytes),
    );
    error!(
        "lqosd memory watchdog diagnostics: flows_tracked={} bus_requests={} time_to_poll_hosts_us={} high_watermark_down={} high_watermark_up={}",
        FLOWS_TRACKED.load(Ordering::Relaxed),
        BUS_REQUESTS.load(Ordering::Relaxed),
        TIME_TO_POLL_HOSTS.load(Ordering::Relaxed),
        HIGH_WATERMARK.get_down(),
        HIGH_WATERMARK.get_up(),
    );
}

#[derive(Debug)]
struct MemoryWatchdogConfig {
    min_available_bytes: u64,
    min_process_bytes: u64,
    max_process_bytes: u64,
    max_swap_bytes: u64,
}

impl MemoryWatchdogConfig {
    fn from_env() -> Self {
        Self {
            min_available_bytes: env_mib(
                "LQOSD_MEMORY_WATCHDOG_MIN_AVAILABLE_MB",
                DEFAULT_MIN_AVAILABLE_BYTES,
            ),
            min_process_bytes: env_mib(
                "LQOSD_MEMORY_WATCHDOG_MIN_PROCESS_MB",
                DEFAULT_MIN_PROCESS_BYTES,
            ),
            max_process_bytes: env_mib(
                "LQOSD_MEMORY_WATCHDOG_MAX_PROCESS_MB",
                DEFAULT_MAX_PROCESS_BYTES,
            ),
            max_swap_bytes: env_mib("LQOSD_MEMORY_WATCHDOG_MAX_SWAP_MB", DEFAULT_MAX_SWAP_BYTES),
        }
    }

    fn critical_reason(&self, snapshot: &MemorySnapshot) -> Option<String> {
        if snapshot.process_swap_bytes >= self.max_swap_bytes {
            return Some(format!(
                "process swap {} reached limit {}",
                format_bytes(snapshot.process_swap_bytes),
                format_bytes(self.max_swap_bytes)
            ));
        }

        if snapshot.process_total_bytes() >= self.max_process_bytes {
            return Some(format!(
                "process rss+swap {} reached limit {}",
                format_bytes(snapshot.process_total_bytes()),
                format_bytes(self.max_process_bytes)
            ));
        }

        if snapshot.mem_available_bytes < self.min_available_bytes
            && snapshot.process_total_bytes() >= self.min_process_bytes
        {
            return Some(format!(
                "available memory {} below limit {} while lqosd uses {}",
                format_bytes(snapshot.mem_available_bytes),
                format_bytes(self.min_available_bytes),
                format_bytes(snapshot.process_total_bytes())
            ));
        }

        None
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MemorySnapshot {
    mem_total_bytes: u64,
    mem_available_bytes: u64,
    process_rss_bytes: u64,
    process_swap_bytes: u64,
    thread_count: u64,
}

impl MemorySnapshot {
    fn read() -> anyhow::Result<Self> {
        let meminfo = parse_proc_key_values(&std::fs::read_to_string("/proc/meminfo")?);
        let status = parse_proc_key_values(&std::fs::read_to_string("/proc/self/status")?);

        Ok(Self {
            mem_total_bytes: read_required_kb(&meminfo, "MemTotal")?,
            mem_available_bytes: read_required_kb(&meminfo, "MemAvailable")?,
            process_rss_bytes: read_required_kb(&status, "VmRSS")?,
            process_swap_bytes: read_optional_kb(&status, "VmSwap"),
            thread_count: read_optional_raw(&status, "Threads"),
        })
    }

    fn process_total_bytes(&self) -> u64 {
        self.process_rss_bytes
            .saturating_add(self.process_swap_bytes)
    }
}

fn parse_proc_key_values(input: &str) -> HashMap<String, u64> {
    input
        .lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let value = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key.to_string(), value))
        })
        .collect()
}

fn read_required_kb(values: &HashMap<String, u64>, key: &str) -> anyhow::Result<u64> {
    values
        .get(key)
        .copied()
        .map(kib)
        .ok_or_else(|| anyhow::anyhow!("{key} missing from /proc memory data"))
}

fn read_optional_kb(values: &HashMap<String, u64>, key: &str) -> u64 {
    values.get(key).copied().map(kib).unwrap_or(0)
}

fn read_optional_raw(values: &HashMap<String, u64>, key: &str) -> u64 {
    values.get(key).copied().unwrap_or(0)
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn env_mib(name: &str, default_bytes: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(mib)
        .unwrap_or(default_bytes)
}

const fn kib(value: u64) -> u64 {
    value * 1024
}

const fn mib(value: u64) -> u64 {
    value * 1024 * 1024
}

const fn gib(value: u64) -> u64 {
    value * 1024 * 1024 * 1024
}

fn format_bytes(bytes: u64) -> String {
    let mib_value = bytes as f64 / mib(1) as f64;
    format!("{mib_value:.1} MiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_parser_reads_kb_and_raw_values() {
        let parsed = parse_proc_key_values(
            "MemTotal:       15458992 kB\nMemAvailable:   12843176 kB\nThreads:\t44\n",
        );

        assert_eq!(
            read_required_kb(&parsed, "MemTotal").unwrap(),
            15_458_992 * 1024
        );
        assert_eq!(read_optional_raw(&parsed, "Threads"), 44);
        assert_eq!(read_optional_kb(&parsed, "VmSwap"), 0);
    }

    #[test]
    fn watchdog_restarts_on_process_swap_limit() {
        let config = MemoryWatchdogConfig {
            min_available_bytes: mib(2_304),
            min_process_bytes: mib(1_024),
            max_process_bytes: gib(4),
            max_swap_bytes: gib(1),
        };
        let snapshot = MemorySnapshot {
            mem_total_bytes: gib(16),
            mem_available_bytes: gib(12),
            process_rss_bytes: mib(900),
            process_swap_bytes: gib(2),
            thread_count: 44,
        };

        assert!(
            config
                .critical_reason(&snapshot)
                .unwrap()
                .contains("process swap")
        );
    }

    #[test]
    fn watchdog_restarts_on_low_available_memory_with_large_lqosd() {
        let config = MemoryWatchdogConfig {
            min_available_bytes: mib(2_304),
            min_process_bytes: mib(1_024),
            max_process_bytes: gib(4),
            max_swap_bytes: gib(1),
        };
        let snapshot = MemorySnapshot {
            mem_total_bytes: gib(16),
            mem_available_bytes: mib(1_900),
            process_rss_bytes: mib(1_500),
            process_swap_bytes: 0,
            thread_count: 44,
        };

        assert!(
            config
                .critical_reason(&snapshot)
                .unwrap()
                .contains("available memory")
        );
    }

    #[test]
    fn watchdog_ignores_low_memory_when_lqosd_is_small() {
        let config = MemoryWatchdogConfig {
            min_available_bytes: mib(2_304),
            min_process_bytes: mib(1_024),
            max_process_bytes: gib(4),
            max_swap_bytes: gib(1),
        };
        let snapshot = MemorySnapshot {
            mem_total_bytes: gib(16),
            mem_available_bytes: mib(1_900),
            process_rss_bytes: mib(256),
            process_swap_bytes: 0,
            thread_count: 44,
        };

        assert!(config.critical_reason(&snapshot).is_none());
    }
}
