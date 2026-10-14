//! Host sensors: GPUs (amdgpu through sysfs, NVIDIA through `nvidia-smi`), CPU load and memory from /proc.
//! Everything is optional: a value the machine does not expose is simply absent from the page.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Default)]
pub struct Gpu {
    pub name: String,
    pub busy_pct: Option<f64>,
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    pub power_cap_w: Option<f64>,
    pub clock_mhz: Option<f64>,
    /// Memory the GPU uses: VRAM, plus GTT (system RAM mapped for the GPU) on integrated / unified-memory chips.
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Host {
    pub gpus: Vec<Gpu>,
    pub cpu_pct: Option<f64>,
    pub cpu_threads: usize,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
}

pub struct Sensors {
    amd: Vec<PathBuf>,
    nvidia: bool,
    prev_cpu: Option<(u64, u64)>,
}

fn read_num(p: &Path) -> Option<f64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

impl Sensors {
    pub fn detect() -> Self {
        let mut amd = Vec::new();
        if let Ok(rd) = fs::read_dir("/sys/class/drm") {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                let is_card = n
                    .strip_prefix("card")
                    .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
                let dev = e.path().join("device");
                if is_card && dev.join("gpu_busy_percent").exists() {
                    amd.push(dev);
                }
            }
        }
        amd.sort();
        let nvidia = Command::new("nvidia-smi")
            .arg("-L")
            .output()
            .is_ok_and(|o| o.status.success());
        Sensors {
            amd,
            nvidia,
            prev_cpu: None,
        }
    }

    pub fn read(&mut self) -> Host {
        let mut gpus: Vec<Gpu> = self.amd.iter().enumerate().map(|(i, d)| amd_gpu(i, d)).collect();
        if self.nvidia {
            gpus.extend(nvidia_gpus());
        }
        let (cpu_pct, cpu_threads) = self.cpu();
        let (mem_used, mem_total) = memory();
        Host {
            gpus,
            cpu_pct,
            cpu_threads,
            mem_used,
            mem_total,
        }
    }

    fn cpu(&mut self) -> (Option<f64>, usize) {
        let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
        let threads = stat
            .lines()
            .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
            .count();
        let line = match stat.lines().next() {
            Some(l) if l.starts_with("cpu ") => l,
            _ => return (None, threads),
        };
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            return (None, threads);
        }
        let idle = v[3] + v[4];
        let total: u64 = v.iter().take(8).sum();
        let pct = self.prev_cpu.and_then(|(pt, pi)| {
            let (dt, di) = (total.saturating_sub(pt), idle.saturating_sub(pi));
            (dt > 0).then(|| 100.0 * (dt - di.min(dt)) as f64 / dt as f64)
        });
        self.prev_cpu = Some((total, idle));
        (pct, threads)
    }
}

fn amd_gpu(i: usize, dev: &Path) -> Gpu {
    let mut g = Gpu {
        name: format!("GPU {i}"),
        busy_pct: read_num(&dev.join("gpu_busy_percent")),
        ..Default::default()
    };
    if let Ok(rd) = fs::read_dir(dev.join("hwmon"))
        && let Some(h) = rd.flatten().next().map(|e| e.path())
    {
        g.temp_c = read_num(&h.join("temp1_input")).map(|x| x / 1000.0);
        g.power_w = read_num(&h.join("power1_average"))
            .or_else(|| read_num(&h.join("power1_input")))
            .map(|x| x / 1e6);
        g.power_cap_w = read_num(&h.join("power1_cap")).map(|x| x / 1e6).filter(|x| *x > 0.0);
    }
    if let Ok(s) = fs::read_to_string(dev.join("pp_dpm_sclk")) {
        g.clock_mhz = s.lines().find(|l| l.trim_end().ends_with('*')).and_then(|l| {
            let v = l.split(':').nth(1)?.trim();
            v.trim_end_matches('*')
                .trim()
                .trim_end_matches("Mhz")
                .trim_end_matches("MHz")
                .trim()
                .parse()
                .ok()
        });
    }
    let n = |f: &str| read_num(&dev.join(f)).map(|x| x as u64);
    let (vu, vt, gu, gt) = (
        n("mem_info_vram_used"),
        n("mem_info_vram_total"),
        n("mem_info_gtt_used"),
        n("mem_info_gtt_total"),
    );
    if vu.is_some() || gu.is_some() {
        g.mem_used = Some(vu.unwrap_or(0) + gu.unwrap_or(0));
        g.mem_total = Some(vt.unwrap_or(0) + gt.unwrap_or(0));
    }
    g
}

fn nvidia_gpus() -> Vec<Gpu> {
    let out = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,utilization.gpu,temperature.gpu,power.draw,power.limit,clocks.sm,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };
    out.lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split(',').map(|x| x.trim()).collect();
            if c.len() < 9 {
                return None;
            }
            let f = |i: usize| c[i].parse::<f64>().ok();
            Some(Gpu {
                name: format!("GPU {} ({})", c[0], c[1]),
                busy_pct: f(2),
                temp_c: f(3),
                power_w: f(4),
                power_cap_w: f(5),
                clock_mhz: f(6),
                mem_used: f(7).map(|x| (x * 1048576.0) as u64),
                mem_total: f(8).map(|x| (x * 1048576.0) as u64),
            })
        })
        .collect()
}

fn memory() -> (Option<u64>, Option<u64>) {
    let s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |k: &str| {
        s.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|x| x * 1024)
    };
    match (kb("MemTotal:"), kb("MemAvailable:")) {
        (Some(t), Some(a)) => (Some(t.saturating_sub(a)), Some(t)),
        (t, _) => (None, t),
    }
}
