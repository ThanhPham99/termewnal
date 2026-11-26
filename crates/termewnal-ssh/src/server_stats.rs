use std::time::Instant;

/// Non-interactive POSIX `sh` probe gathering CPU, uptime, RAM, network, and root disk stats
/// with minimal forks so it works across Linux distributions (including BusyBox/Alpine) and
/// remote login shells (`sh`, `bash`, `zsh`, `fish`, `csh`).
pub const POSIX_SERVER_STATS_COMMAND: &str = concat!(
    "sh -c '",
    "if [ -r /proc/stat ]; then read _ j </proc/stat && echo \"CPU $j\"; fi; ",
    "if [ -r /proc/uptime ]; then read u _ </proc/uptime && echo \"UP $u\"; fi; ",
    "if [ -r /proc/meminfo ]; then mt=0 ma=0 mf=0 mb=0 mc=0 mr=0; ",
    "while read k v _; do case \"$k\" in ",
    "MemTotal:) mt=$v;; MemAvailable:) ma=$v;; MemFree:) mf=$v;; ",
    "Buffers:) mb=$v;; Cached:) mc=$v;; SReclaimable:) mr=$v;; ",
    "esac; done </proc/meminfo; ",
    "echo \"MEM $mt $ma $mf $mb $mc $mr\"; fi; ",
    "if [ -r /proc/net/dev ]; then OLD_IFS=\"$IFS\"; IFS=\": \t\"; ",
    "while read ifc r1 r2 r3 r4 r5 r6 r7 r8 t1 _; do ",
    "case \"$ifc\" in \"\"|Inter-*|face|lo|veth*) continue;; esac; ",
    "case \"$r1\" in [0-9]*) echo \"NIF $ifc $r1 $t1\";; esac; ",
    "done </proc/net/dev; IFS=\"$OLD_IFS\"; fi; ",
    "(LC_ALL=C df -P -k / 2>/dev/null || LC_ALL=C df -k / 2>/dev/null) | ",
    "while read f tot u av cap _; do ",
    "case \"$tot\" in [0-9]*) echo \"DISK $tot $u $av $cap\";; ",
    "*) case \"$f\" in [0-9]*) echo \"DISK $f $tot $u $av\";; esac;; ",
    "esac; done",
    "'"
);

/// Raw kernel counters and capacities from a single remote server stats probe.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawServerStatsSample {
    pub cpu_total_jiffies: Option<u64>,
    pub cpu_idle_jiffies: Option<u64>,
    pub uptime_secs: Option<f64>,
    pub ram_used_kb: Option<u64>,
    pub ram_total_kb: Option<u64>,
    pub ram_percent: Option<f32>,
    pub disk_used_kb: Option<u64>,
    pub disk_total_kb: Option<u64>,
    pub disk_percent: Option<f32>,
    pub net_rx_bytes: Option<u64>,
    pub net_tx_bytes: Option<u64>,
}

/// Processed live telemetry displayed at the bottom of each connected SSH session pane.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerStatsSnapshot {
    pub cpu_percent: Option<f32>,
    pub ram_percent: Option<f32>,
    pub ram_used_kb: Option<u64>,
    pub ram_total_kb: Option<u64>,
    pub disk_percent: Option<f32>,
    pub disk_used_kb: Option<u64>,
    pub disk_total_kb: Option<u64>,
    pub net_rx_bytes_per_sec: Option<u64>,
    pub net_tx_bytes_per_sec: Option<u64>,
}

/// Parses key-value lines emitted by [`POSIX_SERVER_STATS_COMMAND`] into raw server counters.
pub fn parse_raw_server_stats(stdout: &str) -> Result<RawServerStatsSample, String> {
    let mut sample = RawServerStatsSample::default();
    let mut saw_net = false;
    let mut net_rx = 0u64;
    let mut net_tx = 0u64;

    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("CPU") => {
                let fields: Vec<u64> = parts.filter_map(|p| p.parse::<u64>().ok()).collect();
                if fields.len() >= 4 {
                    let total = fields.iter().copied().fold(0u64, u64::saturating_add);
                    let idle = fields[3].saturating_add(fields.get(4).copied().unwrap_or(0));
                    sample.cpu_total_jiffies = Some(total);
                    sample.cpu_idle_jiffies = Some(idle);
                }
            }
            Some("UP") => {
                if let Some(secs) = parts.next().and_then(|p| p.parse::<f64>().ok())
                    && secs.is_finite()
                {
                    sample.uptime_secs = Some(secs);
                }
            }
            Some("MEM") => {
                let nums: Vec<u64> = parts.filter_map(|p| p.parse::<u64>().ok()).collect();
                if nums.len() >= 6 && nums[0] > 0 {
                    let total_kb = nums[0];
                    let mem_avail = if nums[1] > 0 {
                        nums[1]
                    } else {
                        nums[2]
                            .saturating_add(nums[3])
                            .saturating_add(nums[4])
                            .saturating_add(nums[5])
                    };
                    let used_kb = total_kb.saturating_sub(mem_avail.min(total_kb));
                    let pct = ((used_kb as f64 / total_kb as f64) * 100.0).clamp(0.0, 100.0) as f32;
                    sample.ram_total_kb = Some(total_kb);
                    sample.ram_used_kb = Some(used_kb);
                    sample.ram_percent = Some(pct);
                }
            }
            Some("NIF") => {
                let Some(iface) = parts.next() else {
                    continue;
                };
                if iface == "lo" || iface.starts_with("veth") {
                    continue;
                }
                if let (Some(rx), Some(tx)) = (
                    parts.next().and_then(|p| p.parse::<u64>().ok()),
                    parts.next().and_then(|p| p.parse::<u64>().ok()),
                ) {
                    saw_net = true;
                    net_rx = net_rx.saturating_add(rx);
                    net_tx = net_tx.saturating_add(tx);
                }
            }
            Some("DISK") => {
                let tokens: Vec<&str> = parts.collect();
                if let Some(total_kb) = tokens.first().and_then(|p| p.parse::<u64>().ok())
                    && total_kb > 0
                {
                    let used_kb = tokens.get(1).and_then(|p| p.parse::<u64>().ok());
                    let avail_kb = tokens.get(2).and_then(|p| p.parse::<u64>().ok());
                    let cap_pct = tokens.get(3).and_then(|p| {
                        p.trim_end_matches('%')
                            .parse::<f32>()
                            .ok()
                            .map(|v| v.clamp(0.0, 100.0))
                    });
                    let computed_pct = cap_pct.or_else(|| {
                        used_kb.map(|used| {
                            let denom = avail_kb
                                .map(|a| used.saturating_add(a))
                                .filter(|&d| d > 0)
                                .unwrap_or(total_kb);
                            ((used as f64 / denom as f64) * 100.0).clamp(0.0, 100.0) as f32
                        })
                    });
                    sample.disk_total_kb = Some(total_kb);
                    sample.disk_used_kb = used_kb;
                    sample.disk_percent = computed_pct;
                }
            }
            _ => {}
        }
    }

    if saw_net {
        sample.net_rx_bytes = Some(net_rx);
        sample.net_tx_bytes = Some(net_tx);
    }

    if sample.cpu_total_jiffies.is_none()
        && sample.ram_percent.is_none()
        && sample.disk_percent.is_none()
        && sample.net_rx_bytes.is_none()
    {
        return Err("remote probe produced no recognizable server metrics".to_string());
    }

    Ok(sample)
}

/// Stateful per-connection collector computing CPU utilization and network throughput deltas
/// across consecutive [`RawServerStatsSample`] readings.
#[derive(Debug, Default)]
pub struct ServerStatsCollector {
    prev_raw: Option<RawServerStatsSample>,
    prev_instant: Option<Instant>,
    prev_cpu_percent: Option<f32>,
    prev_rx_bps: Option<u64>,
    prev_tx_bps: Option<u64>,
}

impl ServerStatsCollector {
    /// Creates an empty [`ServerStatsCollector`] ready for the first sample.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets accumulated baseline counters (called after transport disconnect/reconnect).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds a new [`RawServerStatsSample`] sampled at `now` and returns the latest metrics.
    pub fn update(&mut self, current: RawServerStatsSample, now: Instant) -> ServerStatsSnapshot {
        let mut cpu_percent = None;
        let mut rx_bps = None;
        let mut tx_bps = None;

        if let (Some(prev), Some(prev_at)) = (&self.prev_raw, self.prev_instant) {
            if let (Some(c_tot), Some(c_idle), Some(p_tot), Some(p_idle)) = (
                current.cpu_total_jiffies,
                current.cpu_idle_jiffies,
                prev.cpu_total_jiffies,
                prev.cpu_idle_jiffies,
            ) && c_tot > p_tot
            {
                let d_total = c_tot - p_tot;
                let d_idle = c_idle.saturating_sub(p_idle).min(d_total);
                let busy = d_total - d_idle;
                cpu_percent = Some(((busy as f64 / d_total as f64) * 100.0).clamp(0.0, 100.0) as f32);
            }

            let dt_secs = match (current.uptime_secs, prev.uptime_secs) {
                (Some(cur_u), Some(prev_u)) if cur_u > prev_u + 0.05 => cur_u - prev_u,
                _ => now.saturating_duration_since(prev_at).as_secs_f64(),
            };

            if dt_secs >= 0.05
                && let (Some(c_rx), Some(c_tx), Some(p_rx), Some(p_tx)) = (
                    current.net_rx_bytes,
                    current.net_tx_bytes,
                    prev.net_rx_bytes,
                    prev.net_tx_bytes,
                )
                && c_rx >= p_rx
                && c_tx >= p_tx
            {
                rx_bps = Some(((c_rx - p_rx) as f64 / dt_secs).round() as u64);
                tx_bps = Some(((c_tx - p_tx) as f64 / dt_secs).round() as u64);
            }
        }

        if cpu_percent.is_some() {
            self.prev_cpu_percent = cpu_percent;
        } else {
            cpu_percent = self.prev_cpu_percent;
        }
        if rx_bps.is_some() && tx_bps.is_some() {
            self.prev_rx_bps = rx_bps;
            self.prev_tx_bps = tx_bps;
        } else {
            rx_bps = self.prev_rx_bps;
            tx_bps = self.prev_tx_bps;
        }

        let snap = ServerStatsSnapshot {
            cpu_percent,
            ram_percent: current.ram_percent,
            ram_used_kb: current.ram_used_kb,
            ram_total_kb: current.ram_total_kb,
            disk_percent: current.disk_percent,
            disk_used_kb: current.disk_used_kb,
            disk_total_kb: current.disk_total_kb,
            net_rx_bytes_per_sec: rx_bps,
            net_tx_bytes_per_sec: tx_bps,
        };

        self.prev_raw = Some(current);
        self.prev_instant = Some(now);
        snap
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn parses_linux_probe_output_and_computes_deltas() {
        let s1 = "\
CPU 1000 0 200 8800 0 0 0 0
UP 100.00
MEM 1000000 420000 100000 50000 250000 20000
NIF lo 999999 999999
NIF veth123 500000 500000
NIF eth0 1048576 524288
DISK 100000000 74000000 26000000 74%
";
        let s2 = "\
CPU 1180 0 220 9600 0 0 0 0
UP 102.00
MEM 1000000 420000 100000 50000 250000 20000
NIF lo 9999999 9999999
NIF eth0 1896448 1134592
DISK 100000000 74000000 26000000 74%
";
        let t0 = Instant::now();
        let mut collector = ServerStatsCollector::new();
        let snap1 = collector.update(parse_raw_server_stats(s1).unwrap(), t0);
        assert_eq!(snap1.cpu_percent, None);
        assert!((snap1.ram_percent.unwrap() - 58.0).abs() < 0.01);
        assert!((snap1.disk_percent.unwrap() - 74.0).abs() < 0.01);
        assert_eq!(snap1.net_rx_bytes_per_sec, None);

        // Delta CPU: total = 1000, idle = 800 -> 20% busy.
        // Delta NET: rx = 847_872 over 2.0s = 423_936 B/s; tx = 610_304 over 2.0s = 305_152 B/s.
        let snap2 = collector.update(
            parse_raw_server_stats(s2).unwrap(),
            t0 + Duration::from_secs(2),
        );
        assert!((snap2.cpu_percent.unwrap() - 20.0).abs() < 0.01);
        assert_eq!(snap2.net_rx_bytes_per_sec, Some(423_936));
        assert_eq!(snap2.net_tx_bytes_per_sec, Some(305_152));
    }

    #[test]
    fn falls_back_when_mem_available_missing_and_rejects_empty() {
        let legacy = "MEM 1000 0 100 50 200 50\nDISK 2000 1000 1000 50%\n";
        let parsed = parse_raw_server_stats(legacy).unwrap();
        assert!((parsed.ram_percent.unwrap() - 60.0).abs() < 0.01);
        assert!(parse_raw_server_stats("   \n\n").is_err());
    }
}
