//! Cargo's recorded launch environment and the executable it identified.
//! A direct launch uses the record only after checking freshness and bytes.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::process::spawn::FullEnv;

use super::runner::Preflight;

pub(super) const JOBSERVER_KEYS: [&str; 3] = ["MAKEFLAGS", "MFLAGS", "CARGO_MAKEFLAGS"];
const PROBE_MAGIC: &[u8] = b"brokkr-launch-probe-v1\0";
const ENV_MAGIC: &[u8] = b"brokkr-launch-env-v1\0";

/// A validated record, with cargo's jobserver variables removed.
#[derive(Debug, Clone)]
pub(crate) struct FrozenEnv(FullEnv);

impl FrozenEnv {
    fn from_record(mut env: FullEnv) -> Self {
        for key in JOBSERVER_KEYS {
            env.remove(OsStr::new(key));
        }
        Self(env)
    }

    /// For a process that outlives the hold (a `brokkr serve` server and its
    /// peer): drop every variable that ties a process to a hold, a run or a
    /// sidecar. The probe ran under the hold and recorded its capability, and
    /// a detached server that kept it could compile under a hold that is not
    /// its own; the same rule `spawn_detached` applies to the supervisor.
    pub(crate) fn detach(&mut self) {
        for key in crate::process::spawn::HOLD_BOUND_ENV {
            self.0.remove(OsStr::new(key));
        }
    }

    pub(crate) fn with_additions<K, V>(
        &self,
        additions: impl IntoIterator<Item = (K, V)>,
    ) -> FullEnv
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.compose(std::env::var_os::<&str>, additions)
    }

    fn compose<K, V>(
        &self,
        live: impl Fn(&'static str) -> Option<OsString>,
        additions: impl IntoIterator<Item = (K, V)>,
    ) -> FullEnv
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut env = self.0.clone();
        env.extend(
            JOBSERVER_KEYS
                .into_iter()
                .filter_map(|key| live(key).map(|v| (key.into(), v))),
        );
        env.extend(
            additions
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        env
    }

    /// An explicit value for a peer plan, such as a serve's carrier token.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        self.0.insert(name.into(), value.into());
    }
}

/// One verified snapshot and the environment cargo computed for it.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    pub(crate) path: PathBuf,
    pub(crate) env: FrozenEnv,
}

#[derive(Debug)]
pub(crate) struct Probe {
    record: PathBuf,
    config_arg: String,
}

impl Probe {
    pub(crate) fn prepare(
        root: &Path,
        env: &FullEnv,
        inline_rustflags: bool,
    ) -> Result<Self, DevError> {
        let (triple, string) = direct_runner(super::runner::preflight_with_rustflags_config(
            root,
            crate::rustflags::EffectiveEnv::full(env),
            inline_rustflags,
        )?)?;
        let helper = std::path::absolute(super::helper::path()?)?;
        let base = crate::user_dirs::launch_dir()
            .ok_or_else(|| {
                DevError::Config("cannot write a launch probe: $HOME is not set".into())
            })?
            .join("probes");
        let base = std::path::absolute(base)?;
        std::fs::create_dir_all(&base)?;
        let record = base.join(format!("{}-{}.probe", std::process::id(), super::unique()));
        let config_arg = super::runner_config(&triple, &helper, "--probe", &record, string)?;
        Ok(Self { record, config_arg })
    }

    pub(crate) fn config_args(&self) -> [String; 2] {
        ["--config".into(), self.config_arg.clone()]
    }

    pub(crate) fn finish(
        &self,
        stdout: &[u8],
        built: &Path,
        snapshot: &Path,
    ) -> Result<FrozenEnv, DevError> {
        require_fresh(stdout)?;
        let bytes = std::fs::read(&self.record).map_err(|error| {
            DevError::Refused(format!("cannot read cargo's launch probe record: {error}"))
        })?;
        let record = Record::parse(&bytes)
            .ok_or_else(|| DevError::Refused("cargo's launch probe record is malformed".into()))?;
        verify_executable(&record.cwd.join(record.exe), built, snapshot)?;
        Ok(FrozenEnv::from_record(record.env))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        super::remove_launch_file(&self.record);
    }
}

fn direct_runner(preflight: Preflight) -> Result<(String, bool), DevError> {
    match preflight {
        Preflight::Ready {
            triple,
            runner,
            string,
        } if runner.is_empty() => Ok((triple, string)),
        Preflight::Ready { runner, .. } => Err(DevError::Refused(format!(
            "a probed launch requires direct execution, but cargo config selects runner {runner:?}"
        ))),
        Preflight::Blocked(why) => Err(DevError::Refused(format!(
            "cannot install the cargo launch probe: {why}"
        ))),
    }
}

struct Record {
    cwd: PathBuf,
    exe: PathBuf,
    env: FullEnv,
}

impl Record {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let mut tokens = tokens(bytes, PROBE_MAGIC)?;
        let mut path = |tag: &[u8]| {
            if tokens.next()? != tag {
                return None;
            }
            let value = tokens.next()?;
            (!value.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(value)))
        };
        let cwd = path(b"cwd")?;
        let exe = path(b"exe")?;
        if !cwd.is_absolute() {
            return None;
        }
        let mut env = FullEnv::new();
        while let Some(tag) = tokens.next() {
            if tag != b"env" {
                return None;
            }
            insert_entry(&mut env, tokens.next()?)?;
        }
        Some(Self { cwd, exe, env })
    }
}

fn tokens<'a>(bytes: &'a [u8], magic: &[u8]) -> Option<impl Iterator<Item = &'a [u8]>> {
    let body = bytes.strip_prefix(magic)?;
    let body = if body.is_empty() {
        body
    } else {
        body.strip_suffix(&[0])?
    };
    let mut parts = body.split(|&byte| byte == 0);
    if body.is_empty() {
        parts.next();
    }
    Some(parts)
}

fn insert_entry(env: &mut FullEnv, entry: &[u8]) -> Option<()> {
    let equal = entry.iter().position(|&byte| byte == b'=')?;
    if equal == 0 {
        return None;
    }
    let key = OsStr::from_bytes(&entry[..equal]);
    let value = OsStr::from_bytes(&entry[equal + 1..]);
    env.insert(key.to_owned(), value.to_owned())
        .is_none()
        .then_some(())
}

fn require_fresh(stdout: &[u8]) -> Result<(), DevError> {
    let mut artifact = false;
    for (index, line) in String::from_utf8_lossy(stdout).lines().enumerate() {
        let record =
            crate::cargo_json::parse_cargo_json_line(line, index + 1).map_err(DevError::Build)?;
        let Some(record) = record else { continue };
        if record["reason"] == "compiler-artifact" {
            artifact = true;
            if record["fresh"].as_bool() != Some(true) {
                return Err(DevError::Refused(format!(
                    "cargo launch probe did not report a fresh artifact for {}",
                    record["target"]["name"]
                        .as_str()
                        .unwrap_or("an unnamed target")
                )));
            }
        }
    }
    if !artifact {
        return Err(DevError::Refused(
            "cargo launch probe reported no compiler artifacts".into(),
        ));
    }
    Ok(())
}

fn verify_executable(probed: &Path, built: &Path, snapshot: &Path) -> Result<(), DevError> {
    let built_meta = std::fs::metadata(built)?;
    let meta = std::fs::metadata(probed)?;
    if !meta.is_file() || (meta.dev(), meta.ino()) != (built_meta.dev(), built_meta.ino()) {
        return Err(DevError::Refused(
            "cargo's launch probe selected a different file from the build".into(),
        ));
    }
    let probe_bytes = BufReader::new(std::fs::File::open(probed)?);
    let snapshot_bytes = BufReader::new(std::fs::File::open(snapshot)?);
    if !same_bytes(probe_bytes, snapshot_bytes)? {
        return Err(DevError::Refused(
            "cargo's launch probe selected bytes different from the snapshot".into(),
        ));
    }
    Ok(())
}

fn same_bytes(mut left: impl BufRead, mut right: impl BufRead) -> std::io::Result<bool> {
    loop {
        let a = left.fill_buf()?;
        let b = right.fill_buf()?;
        if a.is_empty() || b.is_empty() {
            return Ok(a.is_empty() && b.is_empty());
        }
        let n = a.len().min(b.len());
        if a[..n] != b[..n] {
            return Ok(false);
        }
        left.consume(n);
        right.consume(n);
    }
}

/// Write the complete environment privately, before publishing a serve.
pub(crate) fn save_env(path: &Path, env: &FullEnv) -> Result<(), DevError> {
    write_private(path, &encode_env(env)?)
}

pub(crate) fn load_env(path: &Path) -> Result<FullEnv, DevError> {
    let bytes = std::fs::read(path)?;
    decode_env(&bytes)
        .ok_or_else(|| DevError::Refused("the saved launch environment is malformed".into()))
}

fn encode_env(env: &FullEnv) -> Result<Vec<u8>, DevError> {
    let mut bytes = ENV_MAGIC.to_vec();
    for (key, value) in env {
        let (key, value) = (key.as_bytes(), value.as_bytes());
        if key.is_empty() || key.contains(&b'=') || key.contains(&0) || value.contains(&0) {
            return Err(DevError::Refused(
                "the launch environment has an invalid entry".into(),
            ));
        }
        bytes.extend_from_slice(key);
        bytes.push(b'=');
        bytes.extend_from_slice(value);
        bytes.push(0);
    }
    Ok(bytes)
}

fn decode_env(bytes: &[u8]) -> Option<FullEnv> {
    let mut env = FullEnv::new();
    for entry in tokens(bytes, ENV_MAGIC)? {
        insert_entry(&mut env, entry)?;
    }
    Some(env)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), DevError> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

pub(crate) const WITH_EXE_ENV: &str = "BROKKR_WITH_EXE";
pub(crate) const WITH_ARTIFACT_ENV: &str = "BROKKR_WITH_ARTIFACT";

pub(crate) fn peer_additions(launcher: &Path, artifact: &Path) -> [(OsString, OsString); 2] {
    [
        (WITH_EXE_ENV.into(), launcher.as_os_str().to_owned()),
        (WITH_ARTIFACT_ENV.into(), artifact.as_os_str().to_owned()),
    ]
}

fn peer_plan(snapshot: &Path, env: &FrozenEnv) -> Result<Vec<u8>, DevError> {
    super::encode(&super::Plan {
        peer: Some(std::path::absolute(snapshot)?),
        sets: env
            .0
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        ..super::Plan::default()
    })
}

/// Install a peer in a private directory belonging to this launch.
pub(crate) fn place_peer(program: &Program, dir: &Path) -> Result<PathBuf, DevError> {
    let helper = super::helper::path()?;
    let plan = peer_plan(&program.path, &program.env)?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("peer");
    write_private(&dir.join("peer.brokkr-plan"), &plan)?;
    std::fs::copy(helper, &path)?;
    Ok(path)
}

pub(crate) fn peer_directory(snapshot: &Path) -> Result<PathBuf, DevError> {
    let parent = snapshot
        .parent()
        .ok_or_else(|| DevError::Internal("a peer snapshot has no directory".into()))?;
    let name = ".brokkr-launch";
    let dir = parent.join(name);
    Ok(if dir == snapshot {
        parent.join(".brokkr-peer")
    } else {
        dir
    })
}

/// A foreground run's peer plan belongs to that run and is removed on exit.
pub(crate) struct PeerLauncher {
    dir: PathBuf,
    pub(crate) path: PathBuf,
}

impl PeerLauncher {
    pub(crate) fn new(program: &Program) -> Result<Self, DevError> {
        let base = peer_directory(&program.path)?;
        std::fs::create_dir_all(&base)?;
        let dir = base.join(format!("{}-{}", std::process::id(), super::unique()));
        std::fs::create_dir(&dir)?;
        let mut launcher = Self {
            dir,
            path: PathBuf::new(),
        };
        launcher.path = place_peer(program, &launcher.dir)?;
        Ok(launcher)
    }
}

impl Drop for PeerLauncher {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::output::warn(&format!("could not remove peer launch plan: {error}"));
        }
    }
}

#[cfg(test)]
pub(crate) fn test_env() -> FrozenEnv {
    FrozenEnv(FullEnv::new())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::ffi::OsStringExt as _;

    #[test]
    fn probe_preserves_os_bytes_and_resolves_relative_exe() {
        let bytes = b"brokkr-launch-probe-v1\0cwd\0/root/\xff\0exe\0target/a\0env\0K\xff=v\xff=2\0env\0EMPTY=\0";
        let record = Record::parse(bytes).unwrap();
        assert_eq!(
            record.cwd.join(record.exe).as_os_str().as_bytes(),
            b"/root/\xff/target/a"
        );
        assert_eq!(
            record.env[OsStr::from_bytes(b"K\xff")].as_bytes(),
            b"v\xff=2"
        );
        assert!(record.env[OsStr::new("EMPTY")].is_empty());
        assert!(Record::parse(&bytes[..bytes.len() - 1]).is_none());
        for invalid in [
            b"env\0EMPTY=2\0".as_slice(),
            b"env\0=x\0",
            b"env\0BAD\0",
            b"bad\0x\0",
        ] {
            assert!(Record::parse(&[bytes.as_slice(), invalid].concat()).is_none());
        }
        assert!(Record::parse(b"brokkr-launch-probe-v1\0cwd\0relative\0exe\0a\0").is_none());
        assert!(Record::parse(b"brokkr-launch-probe-v1\0cwd\0/root\0").is_none());
    }

    #[test]
    fn every_artifact_must_be_fresh() {
        let fresh = b"{\"reason\":\"compiler-artifact\",\"fresh\":true}\n";
        assert!(require_fresh(&[fresh.as_slice(), fresh].concat()).is_ok());
        for other in [
            b"{\"reason\":\"compiler-artifact\",\"fresh\":false}\n".as_slice(),
            b"{\"reason\":\"compiler-artifact\"}\n",
            b"{\"reason\":\"compiler-artifact\",\"fresh\":\"true\"}\n",
            b"{\"reason\":\"compiler-artifact\",broken\n",
        ] {
            assert!(require_fresh(&[fresh.as_slice(), other].concat()).is_err());
        }
        assert!(require_fresh(b"{\"reason\":\"build-finished\",\"success\":true}\n").is_err());
    }

    #[test]
    fn same_file_and_same_snapshot_bytes_are_both_required() {
        let dir = crate::test_scratch::scratch("launch", "probe-executable");
        let built = dir.join("built");
        let alias = dir.join("alias");
        let snapshot = dir.join("snapshot");
        std::fs::write(&built, b"same").unwrap();
        std::fs::hard_link(&built, &alias).unwrap();
        std::fs::copy(&built, &snapshot).unwrap();
        assert!(verify_executable(&alias, &built, &snapshot).is_ok());
        assert!(verify_executable(&snapshot, &built, &snapshot).is_err());
        std::fs::write(&snapshot, b"diff").unwrap();
        assert!(verify_executable(&alias, &built, &snapshot).is_err());
        std::fs::write(&snapshot, b"same plus").unwrap();
        assert!(verify_executable(&alias, &built, &snapshot).is_err());
    }

    #[test]
    fn a_missing_probe_record_refuses_even_when_cargo_reports_fresh() {
        let dir = crate::test_scratch::scratch("launch", "missing-probe");
        let probe = Probe {
            record: dir.join("unwritten"),
            config_arg: String::new(),
        };
        let fresh = b"{\"reason\":\"compiler-artifact\",\"fresh\":true}\n";
        let result = probe.finish(fresh, &dir.join("built"), &dir.join("snapshot"));
        assert!(
            matches!(result, Err(DevError::Refused(reason)) if reason.contains("probe record"))
        );
    }

    /// A served program outlives the hold its probe ran under, so the hold's
    /// capability and cohort, which the probe recorded, must not reach it.
    #[test]
    fn a_detached_record_carries_nothing_of_the_hold() {
        let mut frozen = FrozenEnv::from_record(
            crate::process::spawn::HOLD_BOUND_ENV
                .iter()
                .map(|key| (OsString::from(key), OsString::from("held")))
                .chain([("CARGO_PKG_NAME".into(), "app".into())])
                .collect(),
        );
        frozen.detach();
        let env = frozen.with_additions(std::iter::empty::<(OsString, OsString)>());
        for key in crate::process::spawn::HOLD_BOUND_ENV {
            assert!(!env.contains_key(OsStr::new(key)), "{key} survived");
        }
        assert_eq!(env.get(OsStr::new("CARGO_PKG_NAME")).map(OsString::as_os_str), Some(OsStr::new("app")));
    }

    #[test]
    fn only_live_jobserver_keys_and_explicit_additions_join_the_record() {
        let frozen = FrozenEnv::from_record(FullEnv::from([
            ("LD_LIBRARY_PATH".into(), "/cargo:/original".into()),
            ("NATIVE".into(), OsString::from_vec(vec![0xff])),
            ("MAKEFLAGS".into(), "cargo's makeflags".into()),
            ("MFLAGS".into(), "cargo's mflags".into()),
            ("CARGO_MAKEFLAGS".into(), "cargo's jobserver".into()),
        ]));
        let live = |key| match key {
            "MAKEFLAGS" => Some("live".into()),
            "CARGO_MAKEFLAGS" => Some("live cargo".into()),
            _ => None,
        };
        let env = frozen.compose(
            live,
            [
                ("LD_LIBRARY_PATH", "/call"),
                ("CARGO_MAKEFLAGS", "explicit"),
            ],
        );
        assert_eq!(
            env,
            FullEnv::from([
                ("LD_LIBRARY_PATH".into(), "/call".into()),
                ("NATIVE".into(), OsString::from_vec(vec![0xff])),
                ("MAKEFLAGS".into(), "live".into()),
                ("CARGO_MAKEFLAGS".into(), "explicit".into()),
            ])
        );
    }

    #[test]
    fn peer_plan_replaces_environment_and_keeps_cwd_and_live_jobserver() {
        let frozen = FrozenEnv(FullEnv::from([(
            "NATIVE".into(),
            OsString::from_vec(vec![0xff]),
        )]));
        assert_eq!(
            peer_plan(Path::new("/snapshot/peer"), &frozen).unwrap(),
            b"brokkr-launch-plan-v1\0exe\0/snapshot/peer\0env\0replace\0keep\0MAKEFLAGS\0keep\0MFLAGS\0keep\0CARGO_MAKEFLAGS\0set\0NATIVE=\xff\0"
        );
    }

    #[test]
    fn saved_environment_round_trips_empty_and_non_utf8_values() {
        let env = FullEnv::from([
            (OsString::from_vec(vec![0xff]), "a=b".into()),
            ("EMPTY".into(), "".into()),
        ]);
        let encoded = encode_env(&env).unwrap();
        assert_eq!(decode_env(&encoded), Some(env));
        assert!(decode_env(&encoded[..encoded.len() - 1]).is_none());
        assert!(decode_env(&[ENV_MAGIC, b"X=1\0X=2\0"].concat()).is_none());
        assert_eq!(decode_env(ENV_MAGIC), Some(FullEnv::new()));
    }

    #[test]
    fn direct_launch_refuses_wrappers_and_blocked_preflight() {
        let ready = |runner| Preflight::Ready {
            triple: "triple".into(),
            runner,
            string: true,
        };
        assert_eq!(
            direct_runner(ready(vec![])).unwrap(),
            ("triple".into(), true)
        );
        assert!(direct_runner(ready(vec!["wrapper".into()])).is_err());
        assert!(direct_runner(Preflight::Blocked("config".into())).is_err());
    }
}
