use crate::output;

/// The audience and output policy for the shared diagnostic pipeline.
#[derive(Debug, Clone, Copy)]
pub(super) enum Reporter {
    Check { commands: bool },
    Clippy,
}

impl Reporter {
    pub(super) fn config_note(self, line: &str) {
        match self {
            Self::Check { .. } => output::detail(line),
            Self::Clippy => output::run_msg(line),
        }
    }

    pub(super) fn diagnostic_invocation(self, shape: &str, command: &str) {
        match self {
            Self::Check { commands } => announce_sweep(shape, Some(command), commands),
            Self::Clippy => output::status(shape),
        }
    }

    pub(super) fn failed_invocation(self, phase: Phase, label: &str, shape: &str, command: &str) {
        if matches!(self, Self::Clippy) {
            output::error(&format!("{}: {shape}", phase_sweep_tag(phase, label)));
            output::error(&format!("failing command: {command}"));
        }
    }

    pub(super) fn allow_notice(
        self,
        blanket: &[crate::config::check::LintName],
        exact_summary: Option<&str>,
    ) {
        if !blanket.is_empty() {
            let names: Vec<&str> = blanket
                .iter()
                .map(crate::config::check::LintName::as_str)
                .collect();
            let line = format!("clippy: allowing {} ([lints] allow)", names.join(", "));
            match self {
                Self::Check { commands: true } => output::run_msg(&line),
                Self::Check { commands: false } | Self::Clippy => output::detail(&line),
            }
        }
        if let Some(summary) = exact_summary {
            let line = format!("clippy: allowing {summary} ([lints] allow_exact)");
            match self {
                Self::Check { commands: true } | Self::Clippy => output::run_msg(&line),
                Self::Check { commands: false } => output::detail(&line),
            }
        }
    }

    pub(super) fn green_line(self, line: &str) {
        if matches!(self, Self::Check { .. }) {
            output::run_msg(line);
        }
    }

    pub(super) fn command_after_success(self, command: &str) {
        if matches!(self, Self::Clippy) {
            output::run_msg(command);
        }
    }
}

use super::{Phase, announce_sweep, phase_sweep_tag};
