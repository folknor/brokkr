//! CPU model and frequency facts read from procfs and sysfs.

/// CPU facts used by the environment report, measurement provenance and bench
/// baseline stamp.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CpuInfo {
    pub model: Option<String>,
    pub physical_cores: Option<u32>,
    pub threads: Option<u32>,
    pub frequency_range: Option<String>,
    pub boost: Option<String>,
    pub governor: Option<String>,
    pub cache_domain_cores: Option<usize>,
}

impl CpuInfo {
    pub(crate) fn summary(&self) -> String {
        let mut details: Vec<String> = Vec::with_capacity(3);
        if let Some((cores, threads)) = self.physical_cores.zip(self.threads) {
            details.push(format!("{cores}c/{threads}t"));
        }
        if let Some(range) = &self.frequency_range {
            details.push(range.clone());
        }
        if let Some(boost) = &self.boost {
            details.push(boost.clone());
        }
        let model = self.model.as_deref().unwrap_or("unknown");
        if details.is_empty() {
            model.to_owned()
        } else {
            format!("{} ({})", model, details.join(", "))
        }
    }
}

pub(crate) fn collect() -> CpuInfo {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let (model, physical_cores, threads) = parse_cpuinfo(&cpuinfo);
    CpuInfo {
        model,
        physical_cores,
        threads,
        frequency_range: read_freq_range(),
        boost: read_boost(),
        governor: read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        cache_domain_cores: crate::cpu_topology::cache_domain_cores(),
    }
}

pub(crate) fn parse_cpuinfo(content: &str) -> (Option<String>, Option<u32>, Option<u32>) {
    let mut model: Option<String> = None;
    let mut cores: Option<u32> = None;
    let mut threads: u32 = 0;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "processor" => threads += 1,
            "model name" if model.is_none() => model = Some(value.trim().to_owned()),
            "cpu cores" if cores.is_none() => cores = value.trim().parse().ok(),
            _ => {}
        }
    }
    let threads = (threads > 0).then_some(threads);
    (model, cores, threads)
}

fn read_freq_range() -> Option<String> {
    let min = read_khz("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq")?;
    let max = read_khz("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")?;
    Some(format!("{}-{} GHz", format_ghz(min), format_ghz(max)))
}

fn read_khz(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub(crate) fn format_ghz(khz: u64) -> String {
    format!("{:.1}", khz as f64 / 1_000_000.0)
}

fn read_boost() -> Option<String> {
    if let Ok(v) = std::fs::read_to_string("/sys/devices/system/cpu/cpufreq/boost") {
        let state = if v.trim() == "0" { "off" } else { "on" };
        return Some(format!("boost {state}"));
    }
    if let Ok(v) = std::fs::read_to_string("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        let state = if v.trim() == "0" { "on" } else { "off" };
        return Some(format!("boost {state}"));
    }
    None
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
