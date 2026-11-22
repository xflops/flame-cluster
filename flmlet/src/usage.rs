//! Best-effort resource usage from the worker's Linux procfs.
use crate::{Host, command, pci};
use flame_api::{ByteUsage, GpuUsage, NodePciDevice, NodeUsage};
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
    pub fn sample(
        &mut self,
        host: &mut impl Host,
        pci_devices: &[NodePciDevice],
    ) -> Option<NodeUsage> {
        let now = Instant::now();
        let cpu = read_text(host, "/proc/stat").and_then(|stat| cpu_counters(&stat));
        let cpu_percent = self
            .previous_cpu
            .filter(|(time, _)| now.duration_since(*time) <= MAX_CPU_SAMPLE_AGE)
            .zip(cpu)
            .and_then(|((_, previous), current)| cpu_utilization(previous, current));
        self.previous_cpu = cpu.map(|counters| (now, counters));

        let memory = read_text(host, "/proc/meminfo")
            .as_deref()
            .and_then(memory_usage);
        let readings = gpu_usage(host, pci_devices);
        let gpu_devices = (!readings.is_empty()).then_some(readings);
        (cpu_percent.is_some() || memory.is_some() || gpu_devices.is_some()).then_some(NodeUsage {
            cpu_percent,
            memory,
            gpu_devices,
        })
    }
}

fn read_text(host: &mut impl Host, path: impl AsRef<Path>) -> Option<String> {
    String::from_utf8(host.read(path.as_ref()).ok()??).ok()
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

fn memory_usage(meminfo: &str) -> Option<ByteUsage> {
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        let mut fields = line.split_ascii_whitespace();
        let target = match fields.next() {
            Some("MemTotal:") => &mut total,
            Some("MemAvailable:") => &mut available,
            _ => continue,
        };
        *target = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|_| fields.next() == Some("kB"))
            .and_then(|value| value.checked_mul(1024));
    }
    let total_bytes = total.filter(|total| *total > 0)?;
    Some(ByteUsage {
        used_bytes: total_bytes.checked_sub(available?)?,
        total_bytes,
    })
}

fn gpu_percent(value: &str) -> Option<u8> {
    value
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 100)
}

fn nvidia_usage(csv: &str) -> impl Iterator<Item = GpuUsage> + '_ {
    csv.lines().filter_map(|line| {
        let (address, percent) = line.split_once(',')?;
        let address = pci::pci_address(address.trim())?;
        // NVML prints eight domain digits even on hosts whose sysfs uses four.
        let (domain, slot) = address.split_once(':')?;
        let domain = u32::from_str_radix(domain, 16).ok()?;
        Some(GpuUsage {
            pci_address: format!("{domain:04x}:{slot}"),
            utilization_percent: gpu_percent(percent)?,
        })
    })
}

fn gpu_usage(host: &mut impl Host, devices: &[NodePciDevice]) -> Vec<GpuUsage> {
    let mut readings = Vec::new();
    let nvidia_smi = Path::new("/usr/bin/nvidia-smi");
    if host.executable(nvidia_smi)
        && let Ok(output) = command(
            host,
            nvidia_smi,
            &[
                "--query-gpu=pci.bus_id,utilization.gpu",
                "--format=csv,noheader,nounits",
            ],
        )
        && output.success
    {
        readings.extend(nvidia_usage(&output.stdout));
    }
    for device in devices
        .iter()
        .filter(|device| device.driver.as_deref() == Some("amdgpu"))
    {
        let Some(address) = pci::pci_address(&device.address) else {
            continue;
        };
        let path = Path::new("/sys/bus/pci/devices")
            .join(&address)
            .join("gpu_busy_percent");
        if let Some(utilization_percent) = read_text(host, path).as_deref().and_then(gpu_percent) {
            readings.push(GpuUsage {
                pci_address: address,
                utilization_percent,
            });
        }
    }
    readings.sort_by(|a, b| a.pci_address.cmp(&b.pci_address));
    readings.dedup_by(|a, b| a.pci_address == b.pci_address);
    readings
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
    fn memory_uses_available_and_ignores_invalid_or_missing_values() {
        assert_eq!(
            memory_usage("MemTotal: 8000 kB\nMemFree: 10 kB\nMemAvailable: 3000 kB"),
            Some(ByteUsage {
                used_bytes: 5000 * 1024,
                total_bytes: 8000 * 1024
            })
        );
        assert_eq!(memory_usage("MemTotal: 1 kB\nMemAvailable: 2 kB"), None);
        assert_eq!(memory_usage("MemTotal: 1 MB\nMemFree: 1 kB"), None);
    }

    #[test]
    fn nvidia_reports_valid_pci_utilization_including_zero() {
        let readings: Vec<_> = nvidia_usage(
            "00000000:01:00.0, 0\n00000002:02:00.0, 86\n00000000:03:00.0, [N/A]\n00000000:04:00.0, 101\nbad, 15",
        ).collect();
        assert_eq!(
            readings,
            [
                GpuUsage {
                    pci_address: "0000:01:00.0".into(),
                    utilization_percent: 0
                },
                GpuUsage {
                    pci_address: "0002:02:00.0".into(),
                    utilization_percent: 86
                },
            ]
        );
    }
}
