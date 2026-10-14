// The lock file's full record codec: every key, emission order and value
// escaping. Included by `lockfile` alone, inside the same module as
// `lock_record.rs`; the standalone rustc guard includes only that file, since
// it reads `auth` and nothing else and would carry this half as dead code.

const PID: &str = "pid";
const STARTTIME: &str = "starttime";
const BOOT_ID: &str = "boot_id";
const ACQ: &str = "acq";
const PID_NS: &str = "pid_ns";
const TIME_NS: &str = "time_ns";
const CODEX_THREAD: &str = "codex_thread";
const CODEX_SESSION: &str = "codex_session";
const DRAINING: &str = "draining";
const CHILD_PID: &str = "child_pid";
const CHILD_STARTTIME: &str = "child_starttime";
const MOCK_PIDS: &str = "mock_pids";
const PROGRESS: &str = "progress";
const PROJECT: &str = "project";
const COMMAND: &str = "command";
const ROOT: &str = "root";
const ARGS: &str = "args";

#[derive(Default)]
pub struct Record {
    pub pid: u32,
    pub starttime: String,
    pub boot_id: String,
    pub acq: String,
    pub pid_ns: String,
    pub time_ns: String,
    pub codex_thread: String,
    pub codex_session: String,
    pub auth: String,
    pub draining: bool,
    pub child: Option<(u32, String)>,
    pub mocks: Vec<(u32, String)>,
    pub progress: Option<(u32, u32)>,
    pub project: String,
    pub command: String,
    pub root: String,
    pub args: String,
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut contents = format!(
            "{PID}={}\n{STARTTIME}={}\n{BOOT_ID}={}\n{ACQ}={}\n{PID_NS}={}\n{TIME_NS}={}\n{CODEX_THREAD}={}\n\
             {CODEX_SESSION}={}\n{AUTH}={}\n{DRAINING}={}\n",
            self.pid,
            self.starttime,
            self.boot_id,
            self.acq,
            self.pid_ns,
            self.time_ns,
            escape_value(&self.codex_thread),
            escape_value(&self.codex_session),
            self.auth,
            u8::from(self.draining),
        );
        match &self.child {
            Some((pid, starttime)) => {
                contents.push_str(&format!("{CHILD_PID}={pid}\n{CHILD_STARTTIME}={starttime}\n"));
            }
            None => {
                contents.push_str(&format!("{CHILD_PID}=\n{CHILD_STARTTIME}=\n"));
            }
        }
        let mocks = self
            .mocks
            .iter()
            .map(|(pid, starttime)| format!("{pid}:{starttime}"))
            .collect::<Vec<_>>()
            .join(",");
        contents.push_str(&format!("{MOCK_PIDS}={mocks}\n"));
        match self.progress {
            Some((run, total)) => contents.push_str(&format!("{PROGRESS}={run}/{total}\n")),
            None => contents.push_str(&format!("{PROGRESS}=\n")),
        }
        contents.push_str(&format!(
            "{PROJECT}={}\n{COMMAND}={}\n{ROOT}={}\n{ARGS}={}\n",
            escape_value(&self.project),
            escape_value(&self.command),
            escape_value(&self.root),
            escape_value(&self.args),
        ));
        contents.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let fields = fields(bytes)?;

        let raw = |key: &str| fields.get(key).map_or("", |value| *value);
        let decoded = |key: &str| unescape_value(raw(key));
        let pid = raw(PID).parse().unwrap_or(0);
        let project = decoded(PROJECT);
        if pid == 0 && project.is_empty() {
            return None;
        }

        let child = raw(CHILD_PID)
            .parse()
            .ok()
            .map(|pid| (pid, raw(CHILD_STARTTIME).to_owned()));
        let mocks = raw(MOCK_PIDS)
            .split(',')
            .filter(|entry| !entry.is_empty())
            .filter_map(|entry| {
                let (pid, starttime) = entry.split_once(':')?;
                Some((pid.trim().parse().ok()?, starttime.trim().to_owned()))
            })
            .collect();
        let progress = raw(PROGRESS)
            .split_once('/')
            .and_then(|(run, total)| Some((run.parse::<u32>().ok()?, total.parse::<u32>().ok()?)));

        Some(Self {
            pid,
            starttime: raw(STARTTIME).to_owned(),
            boot_id: raw(BOOT_ID).to_owned(),
            acq: raw(ACQ).to_owned(),
            pid_ns: raw(PID_NS).to_owned(),
            time_ns: raw(TIME_NS).to_owned(),
            codex_thread: decoded(CODEX_THREAD),
            codex_session: decoded(CODEX_SESSION),
            auth: raw(AUTH).to_owned(),
            draining: raw(DRAINING) == "1",
            child,
            mocks,
            progress,
            project,
            command: decoded(COMMAND),
            root: decoded(ROOT),
            args: decoded(ARGS),
        })
    }
}

pub fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn unescape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            match value.get(index + 1..index + 3) {
                Some("25") => {
                    out.push('%');
                    index += 3;
                    continue;
                }
                Some("0A") => {
                    out.push('\n');
                    index += 3;
                    continue;
                }
                Some("0D") => {
                    out.push('\r');
                    index += 3;
                    continue;
                }
                _ => {}
            }
        }
        let len = value[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&value[index..index + len]);
        index += len;
    }
    out
}
