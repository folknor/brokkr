//! The brokkr invocation a results row records: the exact argv, and the arm
//! key `brokkr results --compare` pairs rows by.
//!
//! Both are captured once, at the CLI boundary, from the argv clap parsed -
//! so the stored argv is exact (no re-quoting of a joined string) and the
//! pairing key never has to be recovered by re-parsing stored text.
//!
//! The arm key is the argv with the tokens that name *provenance or
//! presentation* rather than the benchmark arm removed. The argv is in the key
//! on purpose: `--direct-io` and its kin define genuinely different arms, and
//! collapsing them into one row would silently average two different
//! benchmarks. What leaves it:
//!
//! - `--commit REF` is the whole point of a comparison, not part of the
//!   subject's identity: `--compare A B` asks for the same benchmark at two
//!   commits, and `--commit` is *how* one side got its commit. Keyed on, a
//!   `--commit` row could never pair with the current-tree row it exists to be
//!   compared against.
//! - `--verbose`/`-v` only changes what is printed. It cannot move a wall.
//! - The iteration count of `--bench N` / `--hotpath N` / `--alloc N` (and the
//!   `=N` forms) is how many samples best-of-N drew, not what was measured.
//!   The flag itself stays - the mode is part of the pairing key anyway.
//! - `argv[0]` is reduced to its basename: `brokkr` and `~/.cargo/bin/brokkr`
//!   are the same program to the user.

use crate::db::RunInvocation;

/// This process's invocation, as the results row records it.
pub(crate) fn capture() -> RunInvocation {
    let argv: Vec<String> = std::env::args().collect();
    let arm_key = arm_key(&argv);
    RunInvocation { argv, arm_key }
}

/// The pairing identity of an argv (see the module doc), its kept tokens
/// joined by single spaces.
pub(crate) fn arm_key(argv: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skip_value = false;
    let mut after_count_flag = false;

    for (i, token) in argv.iter().map(String::as_str).enumerate() {
        if skip_value {
            skip_value = false;
            continue;
        }
        if std::mem::take(&mut after_count_flag) && is_iteration_count(token) {
            continue;
        }
        if i == 0 {
            out.push(token.rsplit('/').next().unwrap_or(token));
            continue;
        }
        match token {
            // Space-separated form: the ref is the next token.
            "--commit" => skip_value = true,
            "--verbose" | "-v" => {}
            "--bench" | "--hotpath" | "--alloc" => {
                out.push(token);
                after_count_flag = true;
            }
            _ if token.starts_with("--commit=") => {}
            _ => out.push(strip_count_suffix(token).unwrap_or(token)),
        }
    }
    out.join(" ")
}

/// Whether `token` is a measurement-mode iteration count (all digits).
fn is_iteration_count(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

/// `--bench=3` -> `--bench` (likewise `--hotpath=`/`--alloc=`); `None` for
/// any other token.
fn strip_count_suffix(token: &str) -> Option<&str> {
    let (flag, value) = token.split_once('=')?;
    (matches!(flag, "--bench" | "--hotpath" | "--alloc") && is_iteration_count(value))
        .then_some(flag)
}

#[cfg(test)]
mod tests {
    use super::arm_key;

    fn key(argv: &str) -> String {
        let argv: Vec<String> = argv.split_whitespace().map(str::to_owned).collect();
        arm_key(&argv)
    }

    #[test]
    fn commit_is_not_part_of_the_arm() {
        assert_eq!(
            key("brokkr read --commit abc --bench 3"),
            key("brokkr read --bench 3")
        );
        assert_eq!(key("brokkr read --commit=abc"), "brokkr read");
    }

    #[test]
    fn verbose_and_iteration_counts_are_dropped() {
        assert_eq!(key("brokkr read -v --bench 5"), "brokkr read --bench");
        assert_eq!(
            key("brokkr read --verbose --hotpath=2"),
            "brokkr read --hotpath"
        );
        assert_eq!(
            key("brokkr read --alloc 1 denmark"),
            "brokkr read --alloc denmark"
        );
    }

    #[test]
    fn arm_defining_flags_stay() {
        assert_eq!(
            key("brokkr read --direct-io --bench 3"),
            "brokkr read --direct-io --bench"
        );
        // An unrelated flag's value is not eaten.
        assert_eq!(key("brokkr read --dataset 3"), "brokkr read --dataset 3");
    }

    #[test]
    fn argv0_is_reduced_to_its_basename() {
        assert_eq!(key("/home/u/.cargo/bin/brokkr read"), "brokkr read");
    }

    #[test]
    fn tokens_keep_their_spaces() {
        let argv = vec![
            "brokkr".to_owned(),
            "mogwai".to_owned(),
            "--".to_owned(),
            "a b".to_owned(),
        ];
        assert_eq!(arm_key(&argv), "brokkr mogwai -- a b");
    }
}
