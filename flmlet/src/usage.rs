//! Best-effort resource usage from the worker's Linux procfs.
use crate::Host;
use flame_api::{ByteUsage, NodeUsage};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const MAX_CPU_SAMPLE_AGE: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Default)]
pub struct UsageSampler {
    previous_cpu: Option<(Instant, [u64; 8])>,
}

impl UsageSampler {
    pub fn sample(&mut self, host: &mut impl Host) -> Option<NodeUsage> {
        let now = Instant::now();
        let cpu = read_proc(host, "/proc/stat").and_then(|stat| cpu_counters(&stat));
        let cpu_percent = self
            .previous_cpu
            .filter(|(time, _)| now.duration_since(*time) <= MAX_CPU_SAMPLE_AGE)
            .zip(cpu)
            .and_then(|((_, previous), current)| cpu_utilization(previous, current));
        self.previous_cpu = cpu.map(|counters| (now, counters));

        let (memory, swap) = read_proc(host, "/proc/meminfo")
            .as_deref()
            .map(memory_usage)
            .unwrap_or_default();
        (cpu_percent.is_some() || memory.is_some() || swap.is_some()).then_some(NodeUsage {
            cpu_percent,
            memory,
            swap,
        })
    }
}

fn read_proc(host: &mut impl Host, path: &str) -> Option<String> {
    String::from_utf8(host.read(Path::new(path)).ok()??).ok()
}

fn cpu_counters(stat: &str) -> Option<[u64; 8]> {
    let mut fields = stat.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        (fields.next()? == "cpu").then_some(fields)
    })?;
    // user, nice, system, idle, iowait, irq, softirq, steal.
    // Guest time is already counted in user and nice.
    let mut counters = [0; 8];
    for counter in &mut counters {
        *counter = fields.next()?.parse().ok()?;
    }
    Some(counters)
}

fn cpu_utilization(previous: [u64; 8], current: [u64; 8]) -> Option<f64> {
    let mut total = 0_u64;
    let mut idle = 0_u64;
    for (index, (before, after)) in previous.into_iter().zip(current).enumerate() {
        let delta = after.checked_sub(before)?;
        total = total.checked_add(delta)?;
        if matches!(index, 3 | 4) {
            idle = idle.checked_add(delta)?;
        }
    }
    (total > 0).then(|| (total - idle) as f64 * 100.0 / total as f64)
}

fn memory_usage(meminfo: &str) -> (Option<ByteUsage>, Option<ByteUsage>) {
    let mut memory_total = None;
    let mut memory_available = None;
    let mut swap_total = None;
    let mut swap_free = None;
    for line in meminfo.lines() {
        let mut fields = line.split_ascii_whitespace();
        let target = match fields.next() {
            Some("MemTotal:") => &mut memory_total,
            Some("MemAvailable:") => &mut memory_available,
            Some("SwapTotal:") => &mut swap_total,
            Some("SwapFree:") => &mut swap_free,
            _ => continue,
        };
        *target = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|_| fields.next() == Some("kB"))
            .and_then(|value| value.checked_mul(1024));
    }
    let usage = |total: Option<u64>, available: Option<u64>| {
        let total_bytes = total?;
        Some(ByteUsage {
            used_bytes: total_bytes.checked_sub(available?)?,
            total_bytes,
        })
    };
    (
        usage(memory_total.filter(|total| *total > 0), memory_available),
        usage(swap_total, swap_free),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_only_aggregate_deltas_and_ignores_guest_ticks() {
        let before = cpu_counters("cpu 10 0 0 100 10 0 0 0 7 0\ncpu0 9 0 0").unwrap();
        let after = cpu_counters("cpu 30 0 5 165 20 0 0 0 21 0").unwrap();
        assert_eq!(cpu_utilization(before, after), Some(25.0));
        assert_eq!(cpu_utilization(before, after.map(|_| 0)), None);
        assert_eq!(cpu_utilization(before, before), None);
        assert_eq!(
            cpu_utilization(before, [10, 0, 0, 110, 10, 0, 0, 0]),
            Some(0.0)
        );
        assert!(cpu_counters("cpu0 10 0 0 100 10 0 0 0").is_none());
    }

    #[test]
    fn memory_uses_available_and_preserves_unconfigured_swap() {
        let (memory, swap) = memory_usage(
            "MemTotal: 8000 kB\nMemFree: 10 kB\nMemAvailable: 3000 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB",
        );
        assert_eq!(
            memory,
            Some(ByteUsage {
                used_bytes: 5000 * 1024,
                total_bytes: 8000 * 1024
            })
        );
        assert_eq!(
            swap,
            Some(ByteUsage {
                used_bytes: 0,
                total_bytes: 0
            })
        );
        assert_eq!(
            memory_usage("MemTotal: 1 kB\nMemAvailable: 2 kB"),
            (None, None)
        );
        assert_eq!(
            memory_usage("MemTotal: 1 kB\nMemFree: 1 kB\nSwapTotal: 1 MB\nSwapFree: 0 kB"),
            (None, None)
        );
    }
}
