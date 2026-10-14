//! The runner's typed libtest selection and launch rendering.
use super::Launch;
use crate::error::DevError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TestThreads {
    #[default]
    Default,
    One,
    Count(u32),
}

/// Protocol flags are owned here, never part of a caller's passthrough.
#[derive(Clone, Debug, Default)]
pub(crate) struct LibtestArgs {
    cargo: Vec<String>,
    filters: Vec<String>,
    exact: bool,
    include_ignored: bool,
    nocapture: bool,
    threads: TestThreads,
    passthrough: Vec<String>,
}

impl LibtestArgs {
    pub(crate) fn from_argv(launch: Launch<'_>, args: &[&str]) -> Result<Self, DevError> {
        let mut invocation = Self::default();
        let libtest = if launch == Launch::Cargo {
            let split = args.iter().position(|a| *a == "--").unwrap_or(args.len());
            invocation.cargo = args[..split].iter().map(|a| (*a).to_owned()).collect();
            args.get(split + 1..).unwrap_or_default()
        } else {
            args
        };
        let mut args = libtest.iter().copied();
        while let Some(arg) = args.next() {
            match arg {
                "--exact" => invocation.exact = true,
                "--include-ignored" => invocation.include_ignored = true,
                "--nocapture" => invocation.nocapture = true,
                "--format" => return Err(Self::reserved(arg)),
                "--test-threads" => {
                    let value = args.next().ok_or_else(|| Self::reserved(arg))?;
                    invocation.set_threads(value)?;
                }
                "--skip" | "-Z" | "--color" | "--logfile" | "--shuffle-seed" => {
                    invocation.passthrough.push(arg.into());
                    invocation.passthrough.push(
                        args.next()
                            .ok_or_else(|| DevError::Config(format!("{arg} requires a value")))?
                            .into(),
                    );
                }
                _ if arg.starts_with("--format=") => return Err(Self::reserved(arg)),
                _ if arg.starts_with("--test-threads=") => invocation.set_threads(&arg[15..])?,
                _ if arg.starts_with('-') => invocation.passthrough.push(arg.into()),
                _ => invocation.filters.push(arg.into()),
            }
        }
        Ok(invocation)
    }

    /// Raw user passthrough cannot change runner-owned framing or scheduling.
    pub(crate) fn validate_passthrough(args: &[String]) -> Result<(), DevError> {
        for arg in args {
            let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
            if matches!(flag, "--format" | "--test-threads") {
                return Err(Self::reserved(arg));
            }
        }
        Ok(())
    }

    fn reserved(arg: &str) -> DevError {
        DevError::Config(format!(
            "libtest protocol flag {arg:?} is owned by the runner"
        ))
    }

    fn set_threads(&mut self, value: &str) -> Result<(), DevError> {
        let count = parse_thread_count(value)?;
        self.threads = if count == 1 {
            TestThreads::One
        } else {
            TestThreads::Count(count)
        };
        Ok(())
    }

    pub(crate) fn render(&self, launch: Launch<'_>) -> Vec<String> {
        let mut args = self.cargo.clone();
        if launch == Launch::Cargo {
            args.push("--".into());
        }
        args.extend(self.filters.iter().cloned());
        if self.exact {
            args.push("--exact".into());
        }
        if self.include_ignored {
            args.push("--include-ignored".into());
        }
        if self.nocapture {
            args.push("--nocapture".into());
        }
        args.extend(self.passthrough.iter().cloned());
        match self.threads {
            TestThreads::Default => {}
            TestThreads::One => args.push("--test-threads=1".into()),
            TestThreads::Count(count) => args.push(format!("--test-threads={count}")),
        }
        args.extend(["-Z", "unstable-options", "--format", "json"].map(str::to_owned));
        args
    }
}

pub(super) fn parse_thread_count(value: &str) -> Result<u32, DevError> {
    value.parse::<u32>().map_err(|_| {
        DevError::Config(format!(
            "--test-threads requires a numeric value, got {value:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn protocol_is_last_and_both_format_spellings_are_refused() {
        let launch = Launch::Direct { program: "test" };
        let args = LibtestArgs::from_argv(
            launch,
            &["name", "--exact", "--test-threads", "3", "--skip", "other"],
        )
        .unwrap();
        let rendered = args.render(launch);
        assert_eq!(
            &rendered[rendered.len() - 5..],
            [
                "--test-threads=3",
                "-Z",
                "unstable-options",
                "--format",
                "json"
            ]
        );
        for argv in [vec!["--format", "pretty"], vec!["--format=json"]] {
            assert!(LibtestArgs::from_argv(launch, &argv).is_err());
        }
        for flag in [
            "--format",
            "--format=json",
            "--test-threads",
            "--test-threads=2",
        ] {
            assert!(LibtestArgs::validate_passthrough(&[flag.into()]).is_err());
        }
    }
    #[test]
    fn cargo_flags_are_not_interpreted_as_libtest_flags() {
        let args = LibtestArgs::from_argv(Launch::Cargo, &["test", "--test", "cli", "--", "name"])
            .unwrap();
        assert_eq!(
            &args.render(Launch::Cargo)[..5],
            ["test", "--test", "cli", "--", "name"]
        );
    }
}
