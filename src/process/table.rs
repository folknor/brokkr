use std::collections::HashMap;
use std::io;

use super::{Pid, ProcIdentity, StartTicks};

/// The identity and fields brokkr consumes from one `/proc/<pid>/stat` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    identity: ProcIdentity,
    comm: String,
    state: char,
    parent: Option<Pid>,
    user_ticks: u64,
    system_ticks: u64,
}

impl ProcStat {
    /// Read and parse one process row.
    pub fn read(pid: Pid) -> io::Result<Self> {
        let path = format!("/proc/{}/stat", pid.get());
        let text = std::fs::read_to_string(path)?;
        let row = Self::parse(&text)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat row"))?;
        if row.identity.pid() != pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "proc stat PID did not match requested PID",
            ));
        }
        Ok(row)
    }

    /// Read this process through procfs's `/proc/self` alias.
    pub fn read_self() -> io::Result<Self> {
        let text = std::fs::read_to_string("/proc/self/stat")?;
        Self::parse(&text)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat row"))
    }

    /// Parse one Linux proc stat row. The command field can contain spaces
    /// and parentheses, so its final `)` marks the fixed fields that follow.
    pub fn parse(text: &str) -> Option<Self> {
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        if close < open {
            return None;
        }
        let pid = text[..open].trim().parse::<u32>().ok().and_then(Pid::new)?;
        let comm = text.get(open + 1..close)?.to_owned();
        let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
        if fields.len() < 21 {
            return None;
        }
        let state = fields.first()?.chars().next()?;
        let parent = fields.get(1)?.parse::<u32>().ok().and_then(Pid::new);
        let user_ticks = fields.get(11)?.parse().ok()?;
        let system_ticks = fields.get(12)?.parse().ok()?;
        let start = StartTicks::new(fields.get(19)?.parse().ok()?);
        Some(Self {
            identity: ProcIdentity::new(pid, start),
            comm,
            state,
            parent,
            user_ticks,
            system_ticks,
        })
    }

    pub const fn identity(&self) -> ProcIdentity {
        self.identity
    }

    pub const fn start(&self) -> StartTicks {
        self.identity.start()
    }

    pub fn comm(&self) -> &str {
        &self.comm
    }

    pub const fn state(&self) -> char {
        self.state
    }

    pub const fn parent(&self) -> Option<Pid> {
        self.parent
    }

    pub const fn user_ticks(&self) -> u64 {
        self.user_ticks
    }

    pub const fn system_ticks(&self) -> u64 {
        self.system_ticks
    }
}

/// One best-effort procfs snapshot. It is coherent per row, not atomic across
/// all processes; each row's parent and start identity come from one read.
#[derive(Debug, Default)]
pub struct ProcTable {
    rows: HashMap<Pid, ProcStat>,
}

impl ProcTable {
    /// Capture every readable numeric process entry under `/proc`.
    pub fn snapshot() -> io::Result<Self> {
        let mut rows = HashMap::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .and_then(Pid::new)
            else {
                continue;
            };
            if let Ok(stat) = ProcStat::read(pid) {
                rows.insert(pid, stat);
            }
        }
        Ok(Self { rows })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Pid, &ProcStat)> {
        self.rows.iter()
    }
}
