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
//!
//! Only brokkr's own options leave. The tokens clap assigned to the command's
//! trailing positional (`brokkr mogwai TARGET ARGS...`, the argv brokkr
//! forwards to the child) are values, never brokkr options, so they stay
//! verbatim whatever they spell: a child's `-v`, `--commit X` or `--bench 3`
//! asks for different work. That tail is a suffix of the argv (`trailing_var_arg`
//! takes every token from its first value on, `--` included, and a `last`
//! positional takes everything after `--`, where clap parses no options), so
//! the boundary is one count, [`positional_tail`], taken from the parse.
//!
//! The kept tokens are joined by single spaces, which cannot tell `["a b"]`
//! from `["a", "b"]`. An argv whose kept tokens include an empty one or one
//! with whitespace, or whose first starts with `[`, is keyed as the JSON array
//! `["argv-v1", <tokens>...]` instead; every other argv keys exactly as it
//! always has.
//!
//! That argv key is the *base* key. A row whose measurement depends on a
//! config-resolved selection the argv cannot show carries a per-row qualifier
//! (`BenchConfig::arm_qualifier`), and the writer stores
//! [`qualified_arm_key`] of the two.

use crate::db::RunInvocation;

/// This process's invocation, as the results row records it: the exact argv
/// (not the copy `bare_run_sentinel` rewrites for clap) and how many of its
/// trailing tokens are positional values.
pub(crate) fn capture(argv: Vec<String>, positional_tail: usize) -> RunInvocation {
    let arm_key = arm_key(&argv, positional_tail);
    RunInvocation {
        argv,
        positional_tail,
        arm_key,
        arm_qualifier: None,
    }
}

/// How many trailing tokens of `argv` clap assigned to the parsed leaf
/// command's trailing positional (`trailing_var_arg` or `last`), from the
/// parse itself. Zero when the command has none or it took no values.
///
/// The count is checked: the positional's raw values must be exactly the
/// argv's suffix, else the error names the command. Clap guarantees that for
/// both kinds today; a `value_delimiter` or `value_terminator` on such a
/// positional would break it, and keying on a guessed boundary would pair
/// invocations that differ, so the schema change has to answer it here.
pub(crate) fn positional_tail(
    command: &clap::Command,
    matches: &clap::ArgMatches,
    argv: &[String],
) -> Result<usize, String> {
    let mut command = command;
    let mut matches = matches;
    while let Some((name, sub_matches)) = matches.subcommand() {
        let Some(sub) = command.find_subcommand(name) else {
            return Err(format!("parsed subcommand {name:?} is not in the schema"));
        };
        command = sub;
        matches = sub_matches;
    }

    let mut trailing = command
        .get_positionals()
        .filter(|arg| arg.is_trailing_var_arg_set() || arg.is_last_set());
    let Some(arg) = trailing.next() else {
        return Ok(0);
    };
    if trailing.next().is_some() {
        return Err(format!(
            "`{}` has more than one trailing positional",
            command.get_name()
        ));
    }
    let id = arg.get_id().as_str();
    if matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine) {
        return Ok(0);
    }
    let values: Vec<&std::ffi::OsStr> = match matches.try_get_raw(id) {
        Ok(Some(values)) => values.collect(),
        Ok(None) => return Ok(0),
        Err(error) => return Err(format!("`{}` {id}: {error}", command.get_name())),
    };
    let start = argv.len().checked_sub(values.len());
    let is_suffix = start.is_some_and(|start| {
        start >= 1
            && argv[start..]
                .iter()
                .zip(&values)
                .all(|(token, value)| std::ffi::OsStr::new(token) == *value)
    });
    if !is_suffix {
        return Err(format!(
            "the values of `{}`'s trailing positional `{id}` are not the end of the \
             command line, so the results pairing key cannot tell them from brokkr's \
             own options",
            command.get_name()
        ));
    }
    Ok(values.len())
}

/// The stored pairing identity: the argv's key alone when there is no
/// qualifier (byte-identical to every row before qualifiers existed), else a
/// versioned JSON tuple, so a qualifier has a structural boundary and can
/// never be read as one more argv token.
pub(crate) fn qualified_arm_key(base: &str, qualifier: Option<&str>) -> String {
    match qualifier {
        None => base.to_owned(),
        Some(qualifier) => serde_json::json!(["qualified-v1", base, qualifier]).to_string(),
    }
}

/// The pairing identity of an argv whose last `positional_tail` tokens are
/// positional values (see the module doc).
pub(crate) fn arm_key(argv: &[String], positional_tail: usize) -> String {
    let split = argv.len().saturating_sub(positional_tail).max(1);
    let (options, tail) = argv.split_at(split.min(argv.len()));
    // The prefix is normalized alone, so a pending `--commit` value or count
    // can never consume a positional token.
    let mut kept = without_provenance(options);
    kept.extend(tail.iter().map(String::as_str));
    let ambiguous = kept.first().is_some_and(|first| first.starts_with('['))
        || kept
            .iter()
            .any(|token| token.is_empty() || token.contains(char::is_whitespace));
    if ambiguous {
        let mut tagged = vec!["argv-v1"];
        tagged.extend(kept);
        serde_json::json!(tagged).to_string()
    } else {
        kept.join(" ")
    }
}

/// The key every row stored before the positional boundary and the JSON
/// form existed was given: provenance stripped over the whole argv, child
/// tokens included, joined by single spaces. Only a migration that rewrites
/// those rows' stored argv uses it, so their keys change by the rewrite and
/// nothing else.
pub(crate) fn historical_arm_key(argv: &[String]) -> String {
    without_provenance(argv).join(" ")
}

/// `argv` with brokkr's provenance and presentation tokens removed and
/// `argv[0]` basenamed.
fn without_provenance(argv: &[String]) -> Vec<&str> {
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
    out
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
    use super::{arm_key, historical_arm_key};

    fn argv(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn key(text: &str) -> String {
        arm_key(&argv(text), 0)
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

    /// A positional value is never one of brokkr's options, whatever it
    /// spells: the child's `-v`, `--commit X` and `--bench 3` stay, with or
    /// without a `--` before them.
    #[test]
    fn positional_tokens_are_kept_verbatim() {
        let child = argv("brokkr mogwai stage --bench 5 -- --bench 3 -v --commit x");
        assert_eq!(
            arm_key(&child, 5),
            "brokkr mogwai stage --bench -- --bench 3 -v --commit x"
        );
        let bare = argv("brokkr mogwai stage quick -v");
        assert_eq!(arm_key(&bare, 2), "brokkr mogwai stage quick -v");
        assert_ne!(arm_key(&bare, 2), key("brokkr mogwai stage quick"));
        // Pending option state does not cross into the tail.
        let count = argv("brokkr mogwai stage --bench 5 6");
        assert_eq!(arm_key(&count, 1), "brokkr mogwai stage --bench 6");
    }

    /// Tokens the space join cannot delimit switch the key to JSON, so
    /// `["a b"]` and `["a", "b"]` stay apart; every other key is unchanged.
    #[test]
    fn ambiguous_tokens_are_keyed_as_json() {
        let spaced = vec![
            "brokkr".to_owned(),
            "mogwai".to_owned(),
            "--".to_owned(),
            "a b".to_owned(),
        ];
        let split = argv("brokkr mogwai -- a b");
        assert_eq!(arm_key(&split, 2), "brokkr mogwai -- a b");
        assert_eq!(
            arm_key(&spaced, 1),
            r#"["argv-v1","brokkr","mogwai","--","a b"]"#
        );
        let empty = vec!["brokkr".to_owned(), "mogwai".to_owned(), String::new()];
        assert_eq!(arm_key(&empty, 1), r#"["argv-v1","brokkr","mogwai",""]"#);
        assert!(arm_key(&argv("[odd read"), 0).starts_with(r#"["argv-v1","[odd""#));
    }

    /// The migration key is the one rows were stored with: whole-argv
    /// stripping and a plain join.
    #[test]
    fn historical_key_strips_everywhere() {
        assert_eq!(
            historical_arm_key(&argv("brokkr mogwai stage -- quick -v")),
            "brokkr mogwai stage -- quick"
        );
        let spaced = vec!["brokkr".to_owned(), "a b".to_owned()];
        assert_eq!(historical_arm_key(&spaced), "brokkr a b");
    }
}
