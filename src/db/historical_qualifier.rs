//! Qualifiers for historical typed pbfhogg benchmark rows.

use crate::reservations::PRIVATE_DIR_PLACEHOLDER;

fn read_qualifier(mode: &str) -> String {
    format!("mode={mode}")
}

fn write_qualifier(writer: &str, compression: &str) -> String {
    format!("writer={writer};compression={compression}")
}

fn merge_qualifier(io: &str, compression: &str) -> String {
    format!("io={io};compression={compression}")
}

/// The qualifier a row stored before cells were qualified would carry, from
/// its recorded `cli_args`, or `None` where that string does not prove it.
///
/// `cli_args` is a display string: the program, then each argument, joined
/// by single spaces, an argument holding a space wrapped in double quotes
/// with nothing escaped. So a row whose `cli_args` holds a quote is not
/// recognised, and every other one splits on spaces into exactly the tokens
/// that ran. Only the shapes these benchmarks emitted are accepted, the whole
/// argv after the program, no flag searched for anywhere in it:
///
/// - `bench-read IN --mode M`
/// - `bench-write IN --compression C --writer W`
/// - `bench-merge BASE OSC (-o|--output) OUT --compression C --io-mode I`,
///   then the flags the handle negotiated, in the order it appends them:
///   `--overwrite`, then `--staging-dir D`, then `--tmp-dir D`, each at most
///   once, `D` being the private-directory placeholder or, from builds before
///   per-launch reservations, `OUT`'s directory joined with `.brokkr-pbfhogg`.
///
/// A value is taken as recorded, a retired one (read mode `mmap`, merge I/O
/// `uring-sqpoll`) included, as long as it is a plain token that cannot hold
/// a delimiter.
pub(crate) fn historical_qualifier(command: &str, cli_args: &str) -> Option<String> {
    if cli_args.contains('"') {
        return None;
    }
    let tokens: Vec<&str> = cli_args.split(' ').collect();
    let (program, argv) = tokens.split_first()?;
    if program.is_empty() {
        return None;
    }
    match (command, argv) {
        ("read", ["bench-read", input, "--mode", mode]) if is_path(input) => {
            Some(read_qualifier(value(mode)?))
        }
        (
            "write",
            [
                "bench-write",
                input,
                "--compression",
                compression,
                "--writer",
                writer,
            ],
        ) if is_path(input) => Some(write_qualifier(value(writer)?, value(compression)?)),
        ("merge", ["bench-merge", base, osc, output_flag, output, rest @ ..])
            if is_path(base)
                && is_path(osc)
                && is_path(output)
                && matches!(*output_flag, "-o" | "--output") =>
        {
            let ["--compression", compression, "--io-mode", io, tail @ ..] = rest else {
                return None;
            };
            negotiated_tail(tail, output)?;
            Some(merge_qualifier(value(io)?, value(compression)?))
        }
        _ => None,
    }
}

/// A recorded value that cannot hold a qualifier delimiter or be a flag.
fn value(token: &str) -> Option<&str> {
    let plain = !token.is_empty()
        && !token.starts_with('-')
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.'));
    plain.then_some(token)
}

/// A positional path: present, and not a flag.
fn is_path(token: &str) -> bool {
    !token.is_empty() && !token.starts_with('-')
}

/// Exactly the flags a pbfhogg handle could have appended to a merge cell.
fn negotiated_tail(tail: &[&str], output: &str) -> Option<()> {
    let fixed_dir = match output.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/.brokkr-pbfhogg"),
        Some(_) => "/.brokkr-pbfhogg".to_owned(),
        None => ".brokkr-pbfhogg".to_owned(),
    };
    let private = |dir: &str| dir == PRIVATE_DIR_PLACEHOLDER || dir == fixed_dir;
    let mut rest = tail;
    if let ["--overwrite", after @ ..] = rest {
        rest = after;
    }
    if let ["--staging-dir", dir, after @ ..] = rest {
        if !private(dir) {
            return None;
        }
        rest = after;
    }
    if let ["--tmp-dir", dir, after @ ..] = rest {
        if !private(dir) {
            return None;
        }
        rest = after;
    }
    rest.is_empty().then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shapes_actually_stored_are_recognised() {
        assert_eq!(
            historical_qualifier(
                "read",
                "/p/pbfhogg bench-read /d/planet.osm.pbf --mode mmap"
            )
            .as_deref(),
            Some("mode=mmap")
        );
        assert_eq!(
            historical_qualifier(
                "write",
                "/p/pbfhogg bench-write /d/dk.osm.pbf --compression zlib:6 --writer pipelined"
            )
            .as_deref(),
            Some("writer=pipelined;compression=zlib:6")
        );
        assert_eq!(
            historical_qualifier(
                "merge",
                "/p/pbfhogg bench-merge /d/dk.osm.pbf /d/dk.osc.gz -o /s/out.osm.pbf \
                 --compression zlib --io-mode uring-sqpoll"
            )
            .as_deref(),
            Some("io=uring-sqpoll;compression=zlib")
        );
    }

    /// The negotiated tail, in either era, and nothing else after the cell's
    /// own flags.
    #[test]
    fn a_merge_tail_is_only_what_the_handle_appended() {
        let base = "/p/pbfhogg bench-merge /d/b.osm.pbf /d/c.osc.gz --output /s/out.osm.pbf \
                    --compression none --io-mode buffered";
        for tail in [
            "",
            " --overwrite",
            " --overwrite --staging-dir @brokkr-private-dir@",
            " --staging-dir @brokkr-private-dir@",
            " --overwrite --staging-dir /s/.brokkr-pbfhogg",
            " --overwrite --staging-dir /s/.brokkr-pbfhogg --tmp-dir /s/.brokkr-pbfhogg",
        ] {
            assert_eq!(
                historical_qualifier("merge", &format!("{base}{tail}")).as_deref(),
                Some("io=buffered;compression=none"),
                "{tail}"
            );
        }
        for tail in [
            " --staging-dir /elsewhere/.brokkr-pbfhogg",
            " --staging-dir @brokkr-private-dir@ --overwrite",
            " --overwrite --overwrite",
            " --direct-io",
            " extra",
        ] {
            assert_eq!(
                historical_qualifier("merge", &format!("{base}{tail}")),
                None,
                "{tail}"
            );
        }
    }

    #[test]
    fn anything_unproven_is_left_alone() {
        for (command, cli_args) in [
            // A quoted argument: its boundaries are not recoverable.
            (
                "read",
                "/p/pbfhogg bench-read \"/d/my file.pbf\" --mode parallel",
            ),
            // The wrong command for the subcommand, or a different subcommand.
            ("write", "/p/pbfhogg bench-read /d/x.pbf --mode parallel"),
            ("read", "/p/pbfhogg check --refs /d/x.pbf"),
            // A flag searched for anywhere would match these.
            (
                "read",
                "/p/pbfhogg bench-read /d/x.pbf --mode parallel --direct-io",
            ),
            ("read", "/p/pbfhogg bench-read --mode parallel /d/x.pbf"),
            // A value that could hold a delimiter, or is a flag.
            ("read", "/p/pbfhogg bench-read /d/x.pbf --mode a;b"),
            ("read", "/p/pbfhogg bench-read /d/x.pbf --mode --writer"),
            (
                "write",
                "/p/pbfhogg bench-write /d/x.pbf --writer sync --compression none",
            ),
            ("read", ""),
        ] {
            assert_eq!(historical_qualifier(command, cli_args), None, "{cli_args}");
        }
    }
}
