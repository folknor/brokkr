//! Best-effort CPU bandwidth cap for the default test budget.
//!
//! Walk every visible ancestor: an unlimited leaf can still be limited by
//! its parent. Mount roots can hide further ancestors, which we cannot probe.

use std::path::{Component, Path, PathBuf};

pub(super) fn cpu_budget() -> Option<u32> {
    let membership = std::fs::read_to_string("/proc/thread-self/cgroup").ok()?;
    let mounts = std::fs::read_to_string("/proc/thread-self/mountinfo").ok()?;
    budget_at(&membership, &mounts, |path| {
        std::fs::read_to_string(path).ok()
    })
}

fn parse_max(text: &str) -> Option<u32> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?.parse::<u64>().ok()?;
    let period = fields.next()?.parse::<u64>().ok()?;
    if fields.next().is_some() || quota == 0 || period == 0 {
        return None;
    }
    Some(u32::try_from((quota / period).max(1)).unwrap_or(u32::MAX))
}

// mountinfo escapes whitespace and backslashes with octal sequences.
fn mount_path(text: &str) -> Option<PathBuf> {
    let mut decoded = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let code: String = chars.by_ref().take(3).collect();
            decoded.push(match code.as_str() {
                "040" => ' ',
                "011" => '\t',
                "012" => '\n',
                "134" => '\\',
                _ => return None,
            });
        } else {
            decoded.push(c);
        }
    }
    let path = PathBuf::from(decoded);
    clean_absolute(&path).then_some(path)
}

fn clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

fn budget_at(
    membership: &str,
    mounts: &str,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<u32> {
    let group = Path::new(
        membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?,
    );
    if !clean_absolute(group) {
        return None;
    }
    let mut budget: Option<u32> = None;
    for line in mounts.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        if after.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<_> = before.split_whitespace().collect();
        let Some((root, mount)) = fields
            .get(3)
            .and_then(|r| mount_path(r))
            .zip(fields.get(4).and_then(|m| mount_path(m)))
        else {
            continue;
        };
        let Ok(relative) = group.strip_prefix(&root) else {
            continue;
        };
        let mut current = mount.join(relative);
        loop {
            if let Some(cap) = read(&current.join("cpu.max"))
                .as_deref()
                .and_then(parse_max)
            {
                budget = Some(budget.map_or(cap, |old| old.min(cap)));
            }
            if current == mount || !current.pop() {
                break;
            }
        }
    }
    budget
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_rounds_down_saturates_and_rejects_unusable_values() {
        for (text, expected) in [
            ("250000 100000", Some(2)),
            ("50000 100000", Some(1)),
            ("18446744073709551615 1", Some(u32::MAX)),
            ("max 100000", None),
            ("100 0", None),
            ("0 100", None),
            ("bad", None),
            ("100 10 extra", None),
        ] {
            assert_eq!(parse_max(text), expected, "{text}");
        }
    }

    #[test]
    fn inherited_caps_use_the_mount_root_and_stop_at_the_mount() {
        let mounts = "1 0 0:1 /tenant /cg\\040space rw - cgroup2 cgroup rw\n";
        let cap = budget_at("0::/tenant/parent/leaf\n", mounts, |path| {
            match path.to_str()? {
                "/cg space/parent/leaf/cpu.max" => Some("max 100000".into()),
                "/cg space/parent/cpu.max" => Some("390000 100000".into()),
                "/cg space/cpu.max" => Some("200000 100000".into()),
                _ => panic!("unexpected path: {path:?}"),
            }
        });
        assert_eq!(cap, Some(2));
    }

    #[test]
    fn missing_malformed_or_unrelated_inputs_add_no_cap() {
        let mounts = "1 0 0:1 / /cg rw - cgroup2 cgroup rw\n";
        assert_eq!(budget_at("0::/leaf", mounts, |_| None), None);
        assert_eq!(budget_at("0::/leaf", mounts, |_| Some("bad".into())), None);
        for membership in ["1:cpu:/leaf", "0::/../outside"] {
            assert_eq!(
                budget_at(membership, mounts, |_| panic!("must not read")),
                None
            );
        }
        assert_eq!(
            budget_at(
                "0::/other",
                "1 0 0:1 /tenant /cg rw - cgroup2 cgroup rw",
                |_| panic!("must not read")
            ),
            None
        );
    }
}
