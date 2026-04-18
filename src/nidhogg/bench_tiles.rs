//! Tile serving lifecycle benchmark for nidhogg.
//!
//! Builds nidhogg, starts it as a foreground child with piped stderr, fires
//! tile requests at coordinates derived from the PMTiles bounds, sends SIGTERM,
//! and captures the shutdown KV pairs (tile stats, RSS, peak_rss_kb).

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::pmtiles;

/// RAII guard that kills a child process on drop if it hasn't been consumed.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }

    /// Take ownership of the child, preventing kill-on-drop.
    fn take(&mut self) -> Option<std::process::Child> {
        self.0.take()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

/// Number of iterations over the full tile set per run.
const ITERATIONS: usize = 5;

/// Maximum time to wait for the server to signal readiness on stderr.
/// Generous to accommodate slow disk startup (mmap of large files on HDD).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Check whether a stderr line indicates the server is ready.
///
/// Matches case-insensitively on "listening" - this is more resilient than
/// matching the exact phrase "Listening on" which could change across nidhogg
/// versions.
fn is_ready_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("listening")
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the tile serving lifecycle benchmark.
///
/// Each run: start server → warmup → fire tile requests → SIGTERM → capture
/// KV pairs from stderr. The server's self-reported stats (tile_read_us_p50,
/// tile_bytes_served, peak_rss_kb, etc.) are stored as result KV pairs.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    data_dir: &str,
    tiles: &str,
    port: u16,
    tiles_file: &str,
    tiles_hash: Option<&str>,
    tiles_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    let tile_coords = pmtiles::sample_tile_coords(Path::new(tiles))?;
    if tile_coords.is_empty() {
        return Err(DevError::Config(
            "PMTiles archive contains no tile entries".into(),
        ));
    }
    let min_z = tile_coords.iter().map(|&(z, _, _)| z).min().unwrap_or(0);
    let max_z = tile_coords.iter().map(|&(z, _, _)| z).max().unwrap_or(0);
    output::bench_msg(&format!(
        "z{min_z}-z{max_z}, {} tile coordinates sampled from PMTiles directory",
        tile_coords.len()
    ));

    let mut metadata = vec![
        KvPair::int("meta.port", i64::from(port)),
        KvPair::text("meta.tiles", tiles_file),
        KvPair::real("meta.tiles_mb", tiles_mb),
        #[allow(clippy::cast_possible_wrap)]
        KvPair::int("meta.tile_coords", tile_coords.len() as i64),
        #[allow(clippy::cast_possible_wrap)]
        KvPair::int("meta.iterations", ITERATIONS as i64),
    ];
    if let Some(hash) = tiles_hash {
        metadata.push(KvPair::text("meta.tiles_hash", hash));
    }

    let config = BenchConfig {
        command: "tiles".into(),
        mode: None,
        input_file: Some(tiles_file.to_owned()),
        input_mb: Some(tiles_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs,
        cli_args: None,
        brokkr_args: None,
        metadata,
    };

    let binary = binary.to_owned();
    let data_dir = data_dir.to_owned();
    let tiles = tiles.to_owned();
    let project_root = project_root.to_owned();

    harness.run_internal(&config, |_i| {
        run_lifecycle(
            &binary,
            &data_dir,
            &tiles,
            port,
            &project_root,
            &tile_coords,
        )
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Single lifecycle: start → requests → stop → capture.
#[allow(clippy::too_many_lines)]
fn run_lifecycle(
    binary: &Path,
    data_dir: &str,
    tiles: &str,
    port: u16,
    project_root: &Path,
    tile_coords: &[(u32, u32, u32)],
) -> Result<BenchResult, DevError> {
    let start = Instant::now();

    // 1. Spawn server with piped stderr. ChildGuard ensures cleanup on error.
    let mut guard = ChildGuard::new(spawn_server(binary, data_dir, tiles, port, project_root)?);
    let child = guard
        .0
        .as_mut()
        .ok_or_else(|| DevError::Config("failed to get child process".into()))?;

    // 2. Take stderr and start a reader thread that watches for a ready signal.
    //    After the ready signal, the thread continues reading to EOF so it
    //    captures the shutdown KV pairs emitted after SIGTERM.
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| DevError::Config("failed to capture server stderr".into()))?;

    let (tx, rx) = mpsc::sync_channel::<bool>(1);
    let reader_handle = std::thread::spawn(move || -> (Vec<String>, Vec<u8>) {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut pre_ready = Vec::new();

        // Read lines until the server signals readiness or EOF.
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    tx.send(false).ok();
                    return (pre_ready, Vec::new());
                }
                Ok(_) => {
                    if is_ready_line(&line) {
                        tx.send(true).ok();
                        break;
                    }
                    pre_ready.push(line.clone());
                }
                Err(_) => {
                    tx.send(false).ok();
                    return (pre_ready, Vec::new());
                }
            }
        }

        // Continue reading until EOF (process exit after SIGTERM).
        // This captures the shutdown KV pairs.
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).ok();
        (pre_ready, buf)
    });

    let ready = rx.recv_timeout(STARTUP_TIMEOUT).unwrap_or(false);
    if !ready {
        // Guard handles kill+wait on drop.
        drop(guard);
        let (pre_ready, _) = reader_handle.join().unwrap_or_default();
        let server_output = pre_ready.join("");
        let msg = if server_output.is_empty() {
            format!(
                "nidhogg server did not print ready signal within {}s (no stderr output)",
                STARTUP_TIMEOUT.as_secs()
            )
        } else {
            format!(
                "nidhogg server did not print ready signal within {}s\nserver stderr:\n{}",
                STARTUP_TIMEOUT.as_secs(),
                server_output.chars().take(2000).collect::<String>()
            )
        };
        return Err(DevError::Config(msg));
    }

    output::bench_msg("server ready, firing tile requests");

    // 3. Warmup: one pass through all coordinates.
    for &(z, x, y) in tile_coords {
        curl_get_tile(port, z, x, y)?;
    }

    // 4. Timed passes.
    let total_requests = ITERATIONS * tile_coords.len();
    output::bench_msg(&format!(
        "warmup done, running {ITERATIONS} iterations × {} tiles = {total_requests} requests",
        tile_coords.len()
    ));

    for iter in 0..ITERATIONS {
        for &(z, x, y) in tile_coords {
            curl_get_tile(port, z, x, y)?;
        }
        output::bench_msg(&format!("iteration {}/{ITERATIONS}", iter + 1));
    }

    // 5. Send SIGTERM for graceful shutdown. Take child from guard so we
    //    control the shutdown sequence (guard no longer kills on drop).
    let mut child = guard
        .take()
        .ok_or_else(|| DevError::Config("child process already consumed".into()))?;
    output::bench_msg("sending SIGTERM");
    #[allow(clippy::cast_possible_wrap)]
    let pid = child.id() as i32;
    // SAFETY: sending SIGTERM to our own child process.
    unsafe { libc::kill(pid, libc::SIGTERM) };

    // 6. Wait for process exit, then join reader to get remaining stderr.
    let status = child.wait().map_err(DevError::Io)?;
    let (_, remaining_stderr) = reader_handle.join().unwrap_or_default();

    let elapsed_ms = harness::elapsed_to_ms(&start.elapsed());

    // SIGTERM exit is expected (signal 15 → exit code None on Unix).
    // Only treat non-signal exits as errors.
    if !status.success()
        && let Some(code) = status.code()
    {
        let stderr_preview: String = String::from_utf8_lossy(&remaining_stderr)
            .chars()
            .take(500)
            .collect();
        return Err(DevError::Subprocess {
            program: binary.display().to_string(),
            code: Some(code),
            stderr: stderr_preview,
        });
    }

    // 7. Parse KV pairs from remaining stderr (shutdown output after ready signal).
    let (_stderr_ms, kv) = harness::parse_kv_lines(&remaining_stderr);

    output::bench_msg(&format!(
        "captured {} KV pairs from server shutdown",
        kv.len()
    ));

    Ok(BenchResult {
        elapsed_ms,
        kv,
        distribution: None,
        hotpath: None,
    })
}

/// Spawn nidhogg as a foreground child process with piped stderr.
fn spawn_server(
    binary: &Path,
    data_dir: &str,
    tiles: &str,
    port: u16,
    project_root: &Path,
) -> Result<std::process::Child, DevError> {
    let port_str = port.to_string();

    Command::new(binary)
        .args(["serve", "--data-dir", data_dir, "--tiles", tiles])
        .env("PORT", &port_str)
        .current_dir(project_root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| DevError::Subprocess {
            program: binary.display().to_string(),
            code: None,
            stderr: e.to_string(),
        })
}

/// Fire a single tile GET request. Non-fatal on HTTP errors (tile misses
/// are valid - the server counts them in tile_misses).
fn curl_get_tile(port: u16, z: u32, x: u32, y: u32) -> Result<(), DevError> {
    let url = super::client::tile_url(port, z, x, y);

    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", &url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "curl".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    // Don't fail on HTTP errors (404 = tile miss, still useful data).
    if !output.status.success() {
        // curl itself failed (not an HTTP error), that's a real problem.
        return Err(DevError::Subprocess {
            program: "curl".into(),
            code: output.status.code(),
            stderr: String::new(),
        });
    }

    Ok(())
}
