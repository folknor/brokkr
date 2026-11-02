//! What brokkr's own on-disk state costs, observed, and the one evaluator
//! that turns observations into findings.
//!
//! Two readers take different observations through the same evaluator.
//! `brokkr check` probes only free space (`statvfs` on each store's root, or
//! the nearest existing ancestor of a root not made yet) and never walks a
//! store, so it stays cheap on every run; a filesystem under
//! [`PRESSURE_FREE_PERCENT`] free is reported as a warning naming the stores
//! on it and what shrinks each, and never fails the check. `brokkr env` walks
//! every store for its allocated size as well. Nothing here removes
//! anything; leftovers are `crate::maintenance`'s, retention is each store's.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::clean::layout::ProjectState;
use crate::host::storage::StorageInfo;
use crate::output;

/// A filesystem holding brokkr state is under pressure below this share free.
pub(crate) const PRESSURE_FREE_PERCENT: u64 = 10;

/// What one walk of a store found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Usage {
    /// Allocated bytes (`st_blocks`), each inode counted once in the walk.
    pub bytes: u64,
    /// Some entry could not be read, so `bytes` is a lower bound.
    pub incomplete: bool,
}

/// What measuring a store's root found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Measured {
    /// Nothing at the path.
    Absent,
    /// The root itself could not be read (a permission, a symlink loop); its
    /// size is unknown, not zero.
    Unreadable(String),
    Found(Usage),
}

/// Allocated bytes under `path`. The root is followed if it is a symlink;
/// nothing below it is, and the walk crosses no device boundary (a bind mount
/// on the same device is walked once, by its directory identity). This is
/// what the store occupies, not what removing it would free: another hard
/// link or open descriptor can keep its blocks.
pub(crate) fn usage(path: &Path) -> Measured {
    let root = match std::fs::metadata(path) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Measured::Absent,
        Err(e) => return Measured::Unreadable(e.to_string()),
    };
    let device = root.dev();
    let mut seen = HashSet::new();
    let mut usage = Usage {
        bytes: 0,
        incomplete: false,
    };
    let mut stack = vec![(path.to_path_buf(), root)];
    while let Some((path, meta)) = stack.pop() {
        if meta.dev() != device || !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        usage.bytes = usage
            .bytes
            .saturating_add(meta.blocks().saturating_mul(512));
        if !meta.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            usage.incomplete = true;
            continue;
        };
        for entry in entries {
            match entry.and_then(|e| Ok((e.path(), e.metadata()?))) {
                Ok(next) => stack.push(next),
                Err(_) => usage.incomplete = true,
            }
        }
    }
    Measured::Found(usage)
}

/// One place brokkr keeps state, and what shrinks it.
pub(crate) struct Store {
    pub name: &'static str,
    pub path: PathBuf,
    /// The remedy, or a statement that there is none: a durable record is
    /// never offered up to satisfy the format.
    pub remedy: &'static str,
}

/// brokkr's own state roots: the project's `.brokkr` stores, `~/.brokkr`, and
/// the XDG data and cache directories. `brokkr check` probes these.
pub(crate) fn state_stores(state_root: &Path) -> Vec<Store> {
    let state = ProjectState::new(state_root);
    let mut stores = vec![
        Store {
            name: "verify-reference",
            path: state.verify_reference_dir(),
            remedy: "brokkr clean --verify-reference; verify_reference_budget bounds it",
        },
        Store {
            name: "artifacts",
            path: state.artifacts_dir(),
            remedy: "brokkr clean",
        },
        Store {
            name: "results.db",
            path: state.results_db(),
            remedy: "no pruning: the benchmark record",
        },
        Store {
            name: "sidecar.db",
            path: state.sidecar_db(),
            remedy: "no pruning: the profiler record",
        },
        Store {
            name: "check-logs",
            path: state.check_logs_dir(),
            remedy: "bounded: the newest 10 logs per kind",
        },
        Store {
            name: "accounting",
            path: state.accounting_dir(),
            remedy: "bounded: the newest 10 check runs",
        },
        Store {
            name: "bench",
            path: state.bench_dir(),
            remedy: "no pruning: criterion baselines",
        },
        Store {
            name: "test-hung",
            path: state.test_hung_dir(),
            remedy: "bounded at 16 snapshots; brokkr clean",
        },
        Store {
            name: "corpus-sort",
            path: state.corpus_sort_dir(),
            remedy: "a killed sort's spill goes when dispatch next takes a fresh hold",
        },
        Store {
            name: "library-certifications",
            path: state.library_certifications(),
            remedy: "no pruning: certification records",
        },
        Store {
            name: "litehtml-visual.db",
            path: state.litehtml_visual_db(),
            remedy: "no pruning: visual results and approvals",
        },
        Store {
            name: "sluggrs-visual.db",
            path: state.sluggrs_visual_db(),
            remedy: "no pruning: visual results and approvals",
        },
        Store {
            name: "piners-corpus",
            path: state.piners().corpus_root(),
            remedy: "brokkr clean removes run artefacts; runs.db keeps its newest runs",
        },
        Store {
            name: "ratatoskr-gate.db",
            path: state.ratatoskr().gate_db(),
            remedy: "no pruning: the gate baselines",
        },
    ];
    stores.extend(user_stores());
    stores
}

/// The per-user stores under `~/.brokkr` and the XDG directories that
/// exist on this host.
fn user_stores() -> Vec<Store> {
    let user = [
        (
            "harness",
            crate::user_dirs::harness_dir(),
            "a killed test session's goes when dispatch next takes a fresh hold",
        ),
        (
            "worktrees",
            crate::user_dirs::worktrees(),
            "brokkr clean --worktrees, from each checkout",
        ),
        (
            "sidecar-backups",
            crate::user_dirs::sidecar_backups_dir(),
            "no pruning: three whole copies per project name, delete by hand",
        ),
        (
            "history.db",
            crate::user_dirs::history_db(),
            "no pruning: the command history",
        ),
        (
            "litehtml-prepare",
            crate::user_dirs::litehtml_scripts_dir(),
            "brokkr clean, in a litehtml project",
        ),
    ];
    user.into_iter()
        .filter_map(|(name, path, remedy)| path.map(|path| Store { name, path, remedy }))
        .collect()
}

/// [`state_stores`] and the project's configured trees, for `brokkr env`.
pub(crate) fn all_stores(state_root: &Path, paths: &crate::config::ResolvedPaths) -> Vec<Store> {
    let mut stores = state_stores(state_root);
    stores.extend([
        Store {
            name: "target",
            path: paths.target_dir.clone(),
            remedy: "brokkr clean --cargo",
        },
        Store {
            name: "scratch",
            path: paths.scratch_dir.clone(),
            remedy: "brokkr clean",
        },
        Store {
            name: "data",
            path: paths.data_dir.clone(),
            remedy: "no pruning: the datasets",
        },
    ]);
    stores
}

/// A filesystem under pressure and the stores on it.
pub(crate) struct Pressure<'s> {
    pub storage: StorageInfo,
    pub stores: Vec<&'s Store>,
}

/// The probe of every store: its filesystem when one was found, grouped by
/// mount; and the stores whose filesystem could not be identified, reported
/// as unavailable evidence rather than as free.
pub(crate) struct Probe<'s> {
    pub filesystems: Vec<(StorageInfo, Vec<&'s Store>)>,
    pub unavailable: Vec<&'s Store>,
}

/// Probe each store's filesystem: no walk, one `statvfs` per store.
pub(crate) fn probe(stores: &[Store]) -> Probe<'_> {
    let labelled: Vec<(&str, &Path)> = stores.iter().map(|s| (s.name, s.path.as_path())).collect();
    let found = crate::host::storage::probe_paths(&labelled);
    let mut filesystems: Vec<(StorageInfo, Vec<&Store>)> = Vec::new();
    let mut unavailable = Vec::new();
    for store in stores {
        let Some(info) = found.iter().find(|info| info.label == store.name) else {
            unavailable.push(store);
            continue;
        };
        if info.total_bytes == 0 {
            unavailable.push(store);
            continue;
        }
        let key = (&info.mount_point, &info.device);
        match filesystems
            .iter_mut()
            .find(|(fs, _)| (&fs.mount_point, &fs.device) == key)
        {
            Some((_, on)) => on.push(store),
            None => filesystems.push((info.clone(), vec![store])),
        }
    }
    Probe {
        filesystems,
        unavailable,
    }
}

/// The evaluator: every probed filesystem under [`PRESSURE_FREE_PERCENT`]
/// free.
pub(crate) fn pressure<'s>(probe: &Probe<'s>) -> Vec<Pressure<'s>> {
    probe
        .filesystems
        .iter()
        .filter(|(fs, _)| {
            u128::from(fs.free_bytes) * 100
                < u128::from(fs.total_bytes) * u128::from(PRESSURE_FREE_PERCENT)
        })
        .map(|(fs, stores)| Pressure {
            storage: fs.clone(),
            stores: stores.clone(),
        })
        .collect()
}

/// The finding's lines: the filesystem, then each store on it with its
/// remedy.
pub(crate) fn describe(pressure: &Pressure<'_>) -> Vec<String> {
    let fs = &pressure.storage;
    let mut lines = vec![format!(
        "{} has {:.1}% free ({} of {}), under {PRESSURE_FREE_PERCENT}%; brokkr state there:",
        fs.mount_point,
        crate::host::storage::percent(fs.free_bytes, fs.total_bytes),
        output::format_bytes(fs.free_bytes),
        output::format_bytes(fs.total_bytes),
    )];
    for store in &pressure.stores {
        lines.push(format!(
            "  {} ({}): {}",
            store.name,
            output::path(&store.path),
            store.remedy
        ));
    }
    lines
}

/// `brokkr env`'s block: every store with its allocated size, each
/// filesystem's free space, the stores whose filesystem could not be read,
/// and the same pressure findings `brokkr check` gives.
#[allow(clippy::print_stdout)] // Data output, like the rest of `brokkr env`.
pub(crate) fn print_env(state_root: &Path, paths: &crate::config::ResolvedPaths) {
    let stores = all_stores(state_root, paths);
    let width = stores.iter().map(|s| s.name.len()).max().unwrap_or(0) + 1;
    println!(
        "{:<12} allocated; stores can share files, so the rows do not sum",
        "state:"
    );
    let mut absent = Vec::new();
    for store in &stores {
        let size = match usage(&store.path) {
            Measured::Absent => {
                absent.push(store.name);
                continue;
            }
            Measured::Unreadable(e) => format!("unreadable: {e}"),
            Measured::Found(u) if u.incomplete => {
                format!("at least {}", output::format_bytes(u.bytes))
            }
            Measured::Found(u) => output::format_bytes(u.bytes),
        };
        println!(
            "{:<12} {:<width$}  {size:>12}  {}",
            "",
            format!("{}:", store.name),
            output::path(&store.path),
        );
    }
    if !absent.is_empty() {
        println!("{:<12} absent: {}", "", absent.join(", "));
    }
    let probe = probe(&stores);
    for (fs, on) in &probe.filesystems {
        let names: Vec<&str> = on.iter().map(|s| s.name).collect();
        println!(
            "{:<12} {}  {} free / {} ({:.1}%)  {}",
            "",
            fs.mount_point,
            output::format_bytes(fs.free_bytes),
            output::format_bytes(fs.total_bytes),
            crate::host::storage::percent(fs.free_bytes, fs.total_bytes),
            names.join(", "),
        );
    }
    for store in &probe.unavailable {
        println!("{:<12} {}: filesystem could not be read", "", store.name);
    }
    for finding in pressure(&probe) {
        output::warn(&describe(&finding).join("\n"));
    }
}

/// `brokkr check`'s report: a warning per filesystem under pressure, and
/// sizes left to `brokkr env`. Never fails the check.
pub(crate) fn warn_pressure(state_root: &Path) {
    let stores = state_stores(state_root);
    let probe = probe(&stores);
    for finding in pressure(&probe) {
        let mut lines = describe(&finding);
        lines.push("  sizes: brokkr env".to_owned());
        output::warn(&lines.join("\n"));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// A hard link is counted once, a missing path is no usage at all, and an
    /// empty directory still occupies its own blocks.
    #[test]
    fn usage_counts_each_inode_once_and_absence_as_none() {
        let root = crate::test_scratch::scratch("health", "usage");
        let data = vec![7u8; 64 * 1024];
        std::fs::write(root.join("a"), &data).unwrap();
        let found = |measured| match measured {
            Measured::Found(usage) => usage,
            other => panic!("{other:?}"),
        };
        let single = found(usage(&root));
        std::fs::hard_link(root.join("a"), root.join("b")).unwrap();
        let linked = found(usage(&root));
        assert!(single.bytes >= 64 * 1024, "{single:?}");
        assert!(!single.incomplete);
        // The link adds a directory entry, not a second copy of the blocks.
        assert!(
            linked.bytes < single.bytes + 64 * 1024,
            "{linked:?} {single:?}"
        );
        assert_eq!(usage(&root.join("absent")), Measured::Absent);
    }

    fn info(mount: &str, free: u64, total: u64) -> StorageInfo {
        StorageInfo {
            label: String::new(),
            mount_point: mount.into(),
            device: "/dev/x".into(),
            fstype: "ext4".into(),
            options: String::new(),
            model: None,
            rotational: None,
            free_bytes: free,
            total_bytes: total,
        }
    }

    /// Only a filesystem under the threshold is a finding, with every store
    /// on it.
    #[test]
    fn pressure_is_a_filesystem_under_the_threshold() {
        let stores = [
            Store {
                name: "a",
                path: "/a".into(),
                remedy: "r",
            },
            Store {
                name: "b",
                path: "/b".into(),
                remedy: "r",
            },
        ];
        let probe = Probe {
            filesystems: vec![
                (info("/full", 9, 100), vec![&stores[0], &stores[1]]),
                (info("/roomy", 10, 100), vec![]),
            ],
            unavailable: vec![],
        };
        let found = pressure(&probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].storage.mount_point, "/full");
        let lines = describe(&found[0]);
        assert_eq!(lines.len(), 3, "{lines:?}");
    }
}
