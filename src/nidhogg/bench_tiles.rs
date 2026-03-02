//! Tile serving lifecycle benchmark for nidhogg.
//!
//! Builds nidhogg, starts it as a foreground child with piped stderr, fires
//! tile requests at Copenhagen-area coordinates across z8–z12, sends SIGTERM,
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

// ---------------------------------------------------------------------------
// Tile coordinates — Copenhagen area, z8–z12
// ---------------------------------------------------------------------------

/// (z, x, y) tile coordinates covering Copenhagen at zoom levels 8–12.
/// ~20 tiles spanning different sizes for varied read patterns.
const TILE_COORDS: &[(u32, u32, u32)] = &[
    // z8
    (8, 136, 80), (8, 136, 81), (8, 137, 80), (8, 137, 81),
    // z9
    (9, 272, 160), (9, 273, 160), (9, 272, 161), (9, 273, 161),
    // z10
    (10, 544, 320), (10, 545, 320), (10, 544, 321), (10, 545, 321),
    // z11
    (11, 1088, 640), (11, 1089, 641), (11, 1090, 642), (11, 1091, 643),
    // z12
    (12, 2176, 1280), (12, 2177, 1281), (12, 2178, 1282), (12, 2179, 1283),
];

/// Number of iterations over the full tile set per run.
const ITERATIONS: usize = 5;

/// Maximum time to wait for the server to print "Listening on" to stderr.
/// Generous to accommodate slow disk startup (mmap of large files on HDD).
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the tile serving lifecycle benchmark.
///
/// Each run: start server → warmup → fire tile requests → SIGTERM → capture
/// KV pairs from stderr. The server's self-reported stats (tile_read_us_p50,
/// tile_bytes_served, peak_rss_kb, etc.) are stored as result KV pairs.
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    data_dir: &str,
    tiles: &str,
    port: u16,
    tiles_file: &str,
    tiles_sha256: Option<&str>,
    tiles_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    let mut metadata = vec![
        KvPair::int("meta.port", i64::from(port)),
        KvPair::text("meta.tiles", tiles_file),
        KvPair::real("meta.tiles_mb", tiles_mb),
        #[allow(clippy::cast_possible_wrap)]
        KvPair::int("meta.tile_coords", TILE_COORDS.len() as i64),
        #[allow(clippy::cast_possible_wrap)]
        KvPair::int("meta.iterations", ITERATIONS as i64),
    ];
    if let Some(hash) = tiles_sha256 {
        metadata.push(KvPair::text("meta.tiles_sha256", hash));
    }

    let config = BenchConfig {
        command: "bench tiles".into(),
        variant: None,
        input_file: Some(tiles_file.to_owned()),
        input_mb: Some(tiles_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: None,
        metadata,
    };

    let binary = binary.to_owned();
    let data_dir = data_dir.to_owned();
    let tiles = tiles.to_owned();
    let project_root = project_root.to_owned();

    harness.run_internal(&config, |_i| {
        run_lifecycle(&binary, &data_dir, &tiles, port, &project_root)
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Single lifecycle: start → requests → stop → capture.
fn run_lifecycle(
    binary: &Path,
    data_dir: &str,
    tiles: &str,
    port: u16,
    project_root: &Path,
) -> Result<BenchResult, DevError> {
    let start = Instant::now();

    // 1. Spawn server with piped stderr.
    let mut child = spawn_server(binary, data_dir, tiles, port, project_root)?;

    // 2. Take stderr and start a reader thread that watches for "Listening on".
    //    After the ready signal, the thread continues reading to EOF so it
    //    captures the shutdown KV pairs emitted after SIGTERM.
    let stderr = child.stderr.take().ok_or_else(|| {
        DevError::Config("failed to capture server stderr".into())
    })?;

    let (tx, rx) = mpsc::sync_channel::<bool>(1);
    let reader_handle = std::thread::spawn(move || -> (Vec<String>, Vec<u8>) {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut pre_ready = Vec::new();

        // Read lines until "Listening on" or EOF.
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => { tx.send(false).ok(); return (pre_ready, Vec::new()); }
                Ok(_) => {
                    if line.contains("Listening on") {
                        tx.send(true).ok();
                        break;
                    }
                    pre_ready.push(line.clone());
                }
                Err(_) => { tx.send(false).ok(); return (pre_ready, Vec::new()); }
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
        child.kill().ok();
        child.wait().ok();
        let (pre_ready, _) = reader_handle.join().unwrap_or_default();
        let server_output = pre_ready.join("");
        let msg = if server_output.is_empty() {
            format!(
                "nidhogg server did not print 'Listening on' within {}s (no stderr output)",
                STARTUP_TIMEOUT.as_secs()
            )
        } else {
            format!(
                "nidhogg server did not print 'Listening on' within {}s\nserver stderr:\n{}",
                STARTUP_TIMEOUT.as_secs(),
                server_output.chars().take(2000).collect::<String>()
            )
        };
        return Err(DevError::Config(msg));
    }

    output::bench_msg("server ready, firing tile requests");

    // 3. Warmup: one pass through all coordinates.
    for &(z, x, y) in TILE_COORDS {
        curl_get_tile(port, z, x, y)?;
    }

    // 4. Timed passes.
    let total_requests = ITERATIONS * TILE_COORDS.len();
    output::bench_msg(&format!(
        "warmup done, running {ITERATIONS} iterations × {} tiles = {total_requests} requests",
        TILE_COORDS.len()
    ));

    for iter in 0..ITERATIONS {
        for &(z, x, y) in TILE_COORDS {
            curl_get_tile(port, z, x, y)?;
        }
        output::bench_msg(&format!("iteration {}/{ITERATIONS}", iter + 1));
    }

    // 5. Send SIGTERM for graceful shutdown.
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

    // 7. Parse KV pairs from remaining stderr (shutdown output after "Listening on").
    let (_stderr_ms, kv) = harness::parse_kv_lines(&remaining_stderr);

    output::bench_msg(&format!("captured {} KV pairs from server shutdown", kv.len()));

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
        .args(["serve", data_dir, "--tiles", tiles])
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
/// are valid — the server counts them in tile_misses).
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
