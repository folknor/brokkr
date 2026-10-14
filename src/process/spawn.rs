//! The subprocess construction policy shared by brokkr's spawn choke points.
//!
//! Callers choose a policy by purpose instead of repeating cargo environment,
//! OOM, parent-death, orphan-token, and hold-capability setup. The environment
//! renderer is also used by nextest's generated Cargo config, whose engine
//! builds process commands without exposing them to brokkr.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::cargo_env::{self, Phase, SpawnProfile as CargoProfile};
use crate::error::DevError;
use crate::shutdown::ChildIsolation;

#[derive(Clone, Copy)]
enum ChildKind {
    Plain,
    TestProcess,
    Service,
}

/// The named process policy: `Plain`, `Cargo`, `TestProcess`, `Service`, or a
/// cargo-launched test process (`cargo_test`).
#[derive(Clone, Copy)]
pub(crate) struct ChildPolicy {
    cargo: CargoProfile,
    kind: ChildKind,
}

impl ChildPolicy {
    pub(crate) const fn plain() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::Plain,
        }
    }

    pub(crate) const fn cargo(phase: Option<Phase>) -> Self {
        Self {
            cargo: CargoProfile::Cargo { phase },
            kind: ChildKind::Plain,
        }
    }

    pub(crate) const fn test_process() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::TestProcess,
        }
    }

    pub(crate) const fn service() -> Self {
        Self {
            cargo: CargoProfile::Plain,
            kind: ChildKind::Service,
        }
    }

    pub(crate) const fn cargo_test(phase: Option<Phase>) -> Self {
        Self {
            cargo: CargoProfile::Cargo { phase },
            kind: ChildKind::TestProcess,
        }
    }

    pub(crate) const fn from_cargo_profile(profile: CargoProfile) -> Self {
        match profile {
            CargoProfile::Plain => Self::plain(),
            CargoProfile::Cargo { phase } => Self::cargo(phase),
        }
    }

    /// The brokkr-owned environment carried by this policy. Used when a
    /// subprocess engine, such as nextest, constructs the child itself.
    pub(crate) fn render_env(self) -> Vec<(String, String)> {
        let mut env = crate::hold::child_env();
        if self.carries_orphan_token()
            && let Some((name, value)) = crate::test_orphans::child_env()
        {
            env.push((name.to_owned(), value.to_owned()));
        }
        env
    }

    /// Build and configure one child command. Caller-provided environment is
    /// applied first; brokkr's capability policy is applied last so it cannot
    /// be displaced by a caller pair.
    pub(crate) fn command(
        self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(&str, &str)],
        isolation: ChildIsolation<'_>,
    ) -> Result<Command, DevError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for &(name, value) in env {
            cmd.env(name, value);
        }

        cargo_env::apply(&mut cmd, self.cargo, args, cwd, env)?;
        self.apply_runtime_policy(&mut cmd);

        if isolation.is_process_group() {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        Ok(cmd)
    }

    /// Apply capability and lifecycle policy after the caller finishes its
    /// command environment. Cargo evaluation remains in `command`.
    pub(crate) fn apply_runtime_policy(self, cmd: &mut Command) {
        crate::oom::protect_child(cmd);
        if self.needs_parent_death() {
            crate::shutdown::die_with_parent(cmd);
        }
        if self.carries_orphan_token() {
            crate::test_orphans::stamp(cmd);
        }
        crate::hold::stamp(cmd);
    }

    /// Construct a plain command for callers needing non-text arguments or
    /// custom streams. `output::child::command_output` / `command_status`
    /// apply the policy after all caller configuration.
    pub(crate) fn plain_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
        Command::new(program)
    }

    fn needs_parent_death(self) -> bool {
        matches!(self.kind, ChildKind::TestProcess | ChildKind::Service)
    }

    fn carries_orphan_token(self) -> bool {
        matches!(self.kind, ChildKind::TestProcess | ChildKind::Service)
    }
}

/// Spawn a configured child and publish its PID to the caller immediately.
/// The caller's callback owns the lifetime of that publication and clears it
/// after the child has been reaped.
pub(crate) fn spawn(
    mut command: Command,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<std::process::Child, DevError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let child = command
        .spawn()
        .map_err(|error| DevError::Spawn { program, error })?;
    if let Some(on_spawn) = on_spawn {
        on_spawn(child.id());
    }
    Ok(child)
}

/// Configure a captured command's streams after the common process policy.
pub(crate) fn capture_stdio(command: &mut Command) {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
}
