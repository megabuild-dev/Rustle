//! Host metrics read straight from /proc and cgroup v2; no sysinfo, no `docker stats` streams.

use rustle_types::SystemStats;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
}

fn kb_field(s: &str, name: &str) -> u64 {
    s.lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

pub fn parse_meminfo(s: &str) -> MemInfo {
    MemInfo {
        total_kb: kb_field(s, "MemTotal"),
        available_kb: kb_field(s, "MemAvailable"),
        swap_total_kb: kb_field(s, "SwapTotal"),
        swap_free_kb: kb_field(s, "SwapFree"),
    }
}

/// Returns (idle, total) jiffies of the aggregate `cpu` line.
pub fn parse_cpu(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    if v.len() < 4 {
        return None;
    }
    let idle = v[3] + v.get(4).copied().unwrap_or(0); // idle + iowait
    Some((idle, v.iter().take(8).sum()))
}

pub fn parse_uptime(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|x| x.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

pub fn parse_vmrss_kb(status: &str) -> u64 {
    kb_field(status, "VmRSS")
}

pub fn meminfo(proc_root: &Path) -> MemInfo {
    std::fs::read_to_string(proc_root.join("meminfo"))
        .map(|s| parse_meminfo(&s))
        .unwrap_or_default()
}

pub fn self_rss_kb(proc_root: &Path) -> u64 {
    std::fs::read_to_string(proc_root.join("self/status"))
        .map(|s| parse_vmrss_kb(&s))
        .unwrap_or(0)
}

#[derive(Default)]
pub struct Sampler {
    prev: Option<(u64, u64)>,
}

impl Sampler {
    pub fn cpu_percent(&mut self, proc_root: &Path) -> f32 {
        let cur = std::fs::read_to_string(proc_root.join("stat"))
            .ok()
            .and_then(|s| parse_cpu(&s));
        let Some((idle, total)) = cur else { return 0.0 };
        let pct = match self.prev {
            Some((pi, pt)) if total > pt => {
                let dt = (total - pt) as f32;
                let di = idle.saturating_sub(pi) as f32;
                ((dt - di) / dt * 100.0).clamp(0.0, 100.0)
            }
            _ => 0.0,
        };
        self.prev = Some((idle, total));
        pct
    }
}

/// (total_gb, used_gb) of the filesystem holding `path`.
pub fn disk(path: &Path) -> (f32, f32) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return (0.0, 0.0);
    };
    // SAFETY: `c` is a valid NUL-terminated path and `s` is a properly sized, zeroed out-parameter.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return (0.0, 0.0);
        }
        let bs = s.f_frsize as f64;
        let total = s.f_blocks as f64 * bs;
        let free = s.f_bavail as f64 * bs;
        let gb = 1024.0 * 1024.0 * 1024.0;
        ((total / gb) as f32, ((total - free) / gb) as f32)
    }
}

/// Current memory of a container in MiB, from cgroup v2 `memory.current`. Handles both the systemd
/// and cgroupfs layouts.
pub fn container_mem_mb(cgroup_root: &Path, container_id: &str) -> Option<u32> {
    let candidates = [
        cgroup_root.join(format!(
            "system.slice/docker-{container_id}.scope/memory.current"
        )),
        cgroup_root.join(format!("docker/{container_id}/memory.current")),
    ];
    for p in candidates {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(b) = s.trim().parse::<u64>() {
                return Some((b / (1024 * 1024)) as u32);
            }
        }
    }
    None
}

/// RAM that sites may be allocated in total: physical RAM − baseline used by the OS and the
/// Rustle stack (measured at install, default 250) − a 64 MiB reserve. An explicit override wins.
pub fn budget_mb(mem: &MemInfo, base_used_mb: Option<u32>, override_mb: Option<u32>) -> u32 {
    if let Some(o) = override_mb {
        return o;
    }
    let total = (mem.total_kb / 1024) as u32;
    total
        .saturating_sub(base_used_mb.unwrap_or(250))
        .saturating_sub(64)
}

pub fn fill_system_stats(s: &mut SystemStats, proc_root: &Path, state_dir: &Path) {
    let m = meminfo(proc_root);
    s.mem_total_mb = m.total_kb / 1024;
    s.mem_used_mb = m.total_kb.saturating_sub(m.available_kb) / 1024;
    s.swap_total_mb = m.swap_total_kb / 1024;
    s.swap_used_mb = m.swap_total_kb.saturating_sub(m.swap_free_kb) / 1024;
    s.uptime_secs = std::fs::read_to_string(proc_root.join("uptime"))
        .map(|x| parse_uptime(&x))
        .unwrap_or(0);
    s.rustle_rss_kb = self_rss_kb(proc_root);
    let (t, u) = disk(state_dir);
    s.disk_total_gb = t;
    s.disk_used_gb = u;
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEM: &str = "MemTotal:        1015808 kB\nMemFree:          100000 kB\nMemAvailable:     700000 kB\nSwapTotal:       1572860 kB\nSwapFree:        1500000 kB\n";

    #[test]
    fn meminfo_parse() {
        let m = parse_meminfo(MEM);
        assert_eq!(m.total_kb, 1_015_808);
        assert_eq!(m.available_kb, 700_000);
        assert_eq!(m.swap_total_kb - m.swap_free_kb, 72_860);
        assert_eq!(parse_meminfo("garbage"), MemInfo::default());
    }

    #[test]
    fn cpu_delta() {
        let a = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 1 1 1\n";
        let b = "cpu  200 0 200 1000 0 0 0 0 0 0\n";
        let (i1, t1) = parse_cpu(a).unwrap();
        let (i2, t2) = parse_cpu(b).unwrap();
        assert_eq!((i1, t1, i2, t2), (800, 1000, 1000, 1400));
        let d = tempfile::tempdir().unwrap();
        let mut s = Sampler::default();
        std::fs::write(d.path().join("stat"), a).unwrap();
        assert_eq!(s.cpu_percent(d.path()), 0.0);
        std::fs::write(d.path().join("stat"), b).unwrap();
        // delta total 400, idle 200 => 50% busy
        assert!((s.cpu_percent(d.path()) - 50.0).abs() < 0.01);
        assert!(parse_cpu("nothing").is_none());
    }

    #[test]
    fn uptime_and_rss() {
        assert_eq!(parse_uptime("12345.67 9999.00\n"), 12345);
        assert_eq!(parse_vmrss_kb("Name:\trustle\nVmRSS:\t    6144 kB\n"), 6144);
    }

    #[test]
    fn cgroup_paths_both_layouts() {
        let d = tempfile::tempdir().unwrap();
        let sys = d.path().join("system.slice/docker-abc.scope");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("memory.current"), "104857600\n").unwrap();
        assert_eq!(container_mem_mb(d.path(), "abc"), Some(100));
        let fs = d.path().join("docker/def");
        std::fs::create_dir_all(&fs).unwrap();
        std::fs::write(fs.join("memory.current"), "52428800").unwrap();
        assert_eq!(container_mem_mb(d.path(), "def"), Some(50));
        assert_eq!(container_mem_mb(d.path(), "nope"), None);
    }

    #[test]
    fn budget_rules() {
        let m = parse_meminfo(MEM);
        assert_eq!(budget_mb(&m, None, None), 992 - 250 - 64);
        assert_eq!(budget_mb(&m, Some(300), None), 992 - 300 - 64);
        assert_eq!(budget_mb(&m, Some(300), Some(900)), 900);
        assert_eq!(
            budget_mb(&MemInfo::default(), None, None),
            0,
            "never underflows"
        );
    }

    #[test]
    fn disk_reports_something_for_tmp() {
        let (t, u) = disk(std::env::temp_dir().as_path());
        assert!(t > 0.0 && u >= 0.0 && u <= t);
    }
}
