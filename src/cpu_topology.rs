//! Physical cores sharing one last-level cache - the scheduling island.
//!
//! # What this measures, and why not `nproc`
//!
//! `available_parallelism()` counts logical CPUs: on an SMT part that is twice
//! the physical core count, and on a chiplet part it spans dies that do not
//! share a cache. Neither is the number you want for a test budget. Two tests
//! on the two SMT siblings of one core contend for that core's execution
//! resources, and two tests on separate chiplets pay Infinity-Fabric latency
//! on every shared cache line. Both look like parallelism and neither delivers
//! it linearly.
//!
//! So the unit here is **distinct physical cores within the current thread's
//! affinity and one of its L3 domains**, read from procfs and sysfs:
//!
//! - `Cpus_allowed_list` in `/proc/thread-self/status` gives the CPUs this
//!   thread may run on, including cpuset and affinity restrictions.
//! - The first allowed CPU with an `index3/shared_cpu_list` identifies an L3
//!   domain; its logical CPU list is intersected with the allowed set.
//! - `topology/core_id` collapses SMT siblings onto their physical core.
//!
//! # What it means per vendor
//!
//! The metric is well defined everywhere; what it *names* differs.
//!
//! - **AMD Zen**: an L3 domain is a CCX, commonly a whole CCD - 6 or 8 cores -
//!   and a real boundary, since cross-CCD traffic leaves the die.
//! - **Intel**: there is no CCD. L3 is shared across a ring/mesh, often across
//!   the socket. The returned count is the physical cores in the selected L3
//!   domain that the calling thread may use, so affinity can reduce it on
//!   either vendor.
//! - **Intel hybrid (Alder Lake and later)**: P-cores and E-cores share the
//!   L3, so both are counted when allowed. That is the honest answer for a
//!   cache-sharing question and a slightly optimistic one for a throughput
//!   question, since an E-core is not a P-core. Noted rather than corrected:
//!   sysfs exposes no portable core-class field, and guessing from core counts
//!   would be wrong on the parts that matter.
//!
//! # Fallbacks
//!
//! Every step degrades rather than fails, because this feeds a default and a
//! report, never a correctness decision: no `index3` (no L3, or a container
//! with a trimmed sysfs) falls back to physical cores in the allowed set, and
//! no usable topology or affinity list falls back to `available_parallelism()`.

use std::collections::BTreeSet;
use std::path::Path;

const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// Parse a sysfs CPU list (`"0-3,8,12-15"`) into its members.
///
/// Kept total: a malformed range contributes nothing rather than poisoning
/// the parse, since a partial list still yields a usable count.
fn parse_cpu_list(s: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                if let (Ok(lo), Ok(hi)) = (lo.trim().parse::<u32>(), hi.trim().parse::<u32>())
                    && lo <= hi
                {
                    out.extend(lo..=hi);
                }
            }
            None => {
                if let Ok(n) = part.parse::<u32>() {
                    out.insert(n);
                }
            }
        }
    }
    out
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Collapse a set of logical CPUs onto their distinct physical cores.
///
/// Keyed on `(physical_package_id, core_id)`: `core_id` is only unique within
/// a package, so a two-socket box would otherwise fold socket 1's core 0 into
/// socket 0's.
fn physical_cores(logical: &BTreeSet<u32>) -> usize {
    let mut cores: BTreeSet<(String, String)> = BTreeSet::new();
    for cpu in logical {
        let topo = Path::new(CPU_ROOT).join(format!("cpu{cpu}/topology"));
        let Some(core) = read_trimmed(&topo.join("core_id")) else {
            continue;
        };
        let package =
            read_trimmed(&topo.join("physical_package_id")).unwrap_or_else(|| "0".to_owned());
        cores.insert((package, core));
    }
    cores.len()
}

/// Every logical CPU the kernel reports as online.
fn online_cpus() -> BTreeSet<u32> {
    read_trimmed(&Path::new(CPU_ROOT).join("online"))
        .map(|s| parse_cpu_list(&s))
        .unwrap_or_default()
}

/// Parse the calling thread's allowed logical CPUs from procfs.
fn parse_allowed_cpus(status: &str) -> Option<BTreeSet<u32>> {
    status.lines().find_map(|line| {
        let list = line.strip_prefix("Cpus_allowed_list:")?;
        let cpus = parse_cpu_list(list);
        (!cpus.is_empty()).then_some(cpus)
    })
}

/// The calling thread's affinity, restricted to CPUs sysfs reports online.
/// If the online list is unavailable, keep the kernel-provided affinity list.
fn allowed_cpus() -> Option<BTreeSet<u32>> {
    let status = read_trimmed(Path::new("/proc/thread-self/status"))?;
    let allowed = parse_allowed_cpus(&status)?;
    let online = online_cpus();
    if online.is_empty() {
        return Some(allowed);
    }
    let online_allowed: BTreeSet<u32> = allowed.intersection(&online).copied().collect();
    Some(if online_allowed.is_empty() {
        allowed
    } else {
        online_allowed
    })
}

/// Restrict one cache's logical CPU set to the CPUs this thread may use.
fn allowed_domain(allowed: &BTreeSet<u32>, cache: &BTreeSet<u32>) -> BTreeSet<u32> {
    allowed.intersection(cache).copied().collect()
}

/// Physical cores in one last-level cache domain available to this thread.
///
/// Returns `None` when sysfs says nothing usable, so callers can distinguish
/// "not detected" from a plausible-looking number they would otherwise have
/// to trust.
pub(crate) fn cache_domain_cores() -> Option<usize> {
    let allowed = allowed_cpus()?;
    for cpu in &allowed {
        let l3 = Path::new(CPU_ROOT).join(format!("cpu{cpu}/cache/index3/shared_cpu_list"));
        if let Some(list) = read_trimmed(&l3) {
            let domain = allowed_domain(&allowed, &parse_cpu_list(&list));
            let n = physical_cores(&domain);
            if n > 0 {
                return Some(n);
            }
        }
    }
    // No L3 (or no sysfs cache tree): erase the cache boundary while retaining
    // the calling thread's affinity restriction.
    let n = physical_cores(&allowed);
    (n > 0).then_some(n)
}

/// The cache domain's core count, or a usable number regardless.
///
/// The last resort is `available_parallelism()`, which can overcount on an SMT
/// part - accepted because a default that is too high is a slow test run,
/// while a default of zero is no test run at all.
pub(crate) fn cache_domain_cores_or_default() -> u32 {
    let n = cache_domain_cores().unwrap_or_else(available_parallelism_or_default);
    u32::try_from(n).unwrap_or(u32::MAX).max(1)
}

/// Threads this process may run in parallel, or a fixed guess of 4 when the
/// platform cannot say. The one place that guess lives.
pub(crate) fn available_parallelism_or_default() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
}

/// How to describe the domain in a report, given the CPU model string.
///
/// Vendor-sniffed from the model name because sysfs has no field that says
/// "this L3 is a chiplet". Cosmetic only - it labels a number, never derives
/// one - so a miss costs a slightly wrong word and nothing else.
pub(crate) fn domain_label(cpu_model: &str) -> &'static str {
    let m = cpu_model.to_ascii_lowercase();
    if m.contains("amd") || m.contains("ryzen") || m.contains("epyc") || m.contains("threadripper")
    {
        "CCX/CCD"
    } else if m.contains("intel") || m.contains("xeon") || m.contains("core i") {
        "die"
    } else {
        "L3 domain"
    }
}

#[cfg(test)]
mod cpu_topology_tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_in_every_sysfs_shape() {
        assert_eq!(parse_cpu_list("0-3"), (0..=3).collect());
        assert_eq!(parse_cpu_list("5"), [5].into_iter().collect());
        assert_eq!(
            parse_cpu_list("0-1,4,6-7"),
            [0, 1, 4, 6, 7].into_iter().collect()
        );
        assert_eq!(parse_cpu_list(" 0-2 \n"), (0..=2).collect());
    }

    // Total by design: a list this cannot understand yields fewer CPUs, not a
    // panic and not a poisoned parse of the parts it did understand.
    #[test]
    fn a_malformed_list_degrades_rather_than_failing() {
        assert!(parse_cpu_list("").is_empty());
        assert!(parse_cpu_list("garbage").is_empty());
        assert_eq!(parse_cpu_list("3-1"), BTreeSet::new());
        assert_eq!(
            parse_cpu_list("0-1,junk,4"),
            [0, 1, 4].into_iter().collect()
        );
    }

    #[test]
    fn proc_status_affinity_list_is_parsed() {
        let status = "Name:\tbrokkr\nCpus_allowed:\t000000ff\nCpus_allowed_list:\t4-7,12,15\n";
        assert_eq!(
            parse_allowed_cpus(status),
            Some([4, 5, 6, 7, 12, 15].into_iter().collect())
        );
        assert_eq!(parse_allowed_cpus("Name:\tbrokkr\n"), None);
    }

    #[test]
    fn a_cache_domain_is_limited_to_affinity_allowed_cpus() {
        let allowed: BTreeSet<u32> = [4, 5, 12, 13].into_iter().collect();
        let cache: BTreeSet<u32> = [0, 1, 2, 3, 4, 5, 6, 7].into_iter().collect();
        assert_eq!(
            allowed_domain(&allowed, &cache),
            [4, 5].into_iter().collect()
        );
    }

    #[test]
    fn the_label_follows_the_vendor_and_falls_back_neutrally() {
        assert_eq!(
            domain_label("AMD Ryzen 5 5600G with Radeon Graphics"),
            "CCX/CCD"
        );
        assert_eq!(domain_label("AMD EPYC 7763 64-Core Processor"), "CCX/CCD");
        assert_eq!(domain_label("Intel(R) Xeon(R) Gold 6248"), "die");
        assert_eq!(domain_label("Apple M2"), "L3 domain");
    }

    // The default feeds a test budget, so it must be usable on any machine
    // this runs on - never zero, whatever sysfs did or did not say.
    #[test]
    fn the_default_is_always_at_least_one() {
        assert!(cache_domain_cores_or_default() >= 1);
    }
}
