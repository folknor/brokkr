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
//! So the unit here is **distinct physical cores within cpu0's L3 domain**,
//! read from sysfs:
//!
//! - `cache/index3/shared_cpu_list` gives the logical CPUs sharing that L3.
//! - `topology/core_id` collapses SMT siblings onto their physical core.
//!
//! # What it means per vendor
//!
//! The metric is well defined everywhere; what it *names* differs.
//!
//! - **AMD Zen**: the L3 domain is a CCX, which on current parts is a whole
//!   CCD - 6 or 8 cores. This is the number people mean by "a full CCD", and
//!   it is a real boundary: cross-CCD traffic leaves the die.
//! - **Intel**: there is no CCD. L3 is shared across the whole ring/mesh, so
//!   this returns every physical core in the socket - the die *is* the domain.
//!   The analogous boundary on a multi-socket Intel box is the socket, and
//!   this reports it correctly for the same reason.
//! - **Intel hybrid (Alder Lake and later)**: P-cores and E-cores share the
//!   L3, so both are counted. That is the honest answer for a cache-sharing
//!   question and a slightly optimistic one for a throughput question, since
//!   an E-core is not a P-core. Noted rather than corrected: sysfs exposes no
//!   portable core-class field, and guessing from core counts would be
//!   wrong on the parts that matter.
//!
//! # Fallbacks
//!
//! Every step degrades rather than fails, because this feeds a default and a
//! report, never a correctness decision: no `index3` (no L3, or a container
//! with a trimmed sysfs) falls back to physical cores across the machine, and
//! no topology at all falls back to `available_parallelism()`.

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
        let package = read_trimmed(&topo.join("physical_package_id"))
            .unwrap_or_else(|| "0".to_owned());
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

/// Physical cores sharing cpu0's last-level cache.
///
/// Returns `None` when sysfs says nothing usable, so callers can distinguish
/// "not detected" from a plausible-looking number they would otherwise have
/// to trust.
pub(crate) fn cache_domain_cores() -> Option<usize> {
    let l3 = Path::new(CPU_ROOT).join("cpu0/cache/index3/shared_cpu_list");
    if let Some(list) = read_trimmed(&l3) {
        let n = physical_cores(&parse_cpu_list(&list));
        if n > 0 {
            return Some(n);
        }
    }
    // No L3 (or no sysfs cache tree): fall back to physical cores machine-wide,
    // which is the same question with the cache boundary erased.
    let n = physical_cores(&online_cpus());
    (n > 0).then_some(n)
}

/// The cache domain's core count, or a usable number regardless.
///
/// The last resort is `available_parallelism()`, which overcounts on an SMT
/// part - accepted because a default that is too high is a slow test run,
/// while a default of zero is no test run at all.
pub(crate) fn cache_domain_cores_or_default() -> u32 {
    let n = cache_domain_cores().unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
    });
    u32::try_from(n).unwrap_or(u32::MAX).max(1)
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
        assert_eq!(parse_cpu_list("0-1,junk,4"), [0, 1, 4].into_iter().collect());
    }

    #[test]
    fn the_label_follows_the_vendor_and_falls_back_neutrally() {
        assert_eq!(domain_label("AMD Ryzen 5 5600G with Radeon Graphics"), "CCX/CCD");
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
