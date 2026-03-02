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

/// Number of iterations over the full tile set per run.
const ITERATIONS: usize = 5;

// ---------------------------------------------------------------------------
// Tile coordinate generation from PMTiles bounds
// ---------------------------------------------------------------------------

fn lon_to_tile_x(lon: f64, z: u32) -> u32 {
    let n = f64::from(1u32 << z);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    { ((lon + 180.0) / 360.0 * n).floor() as u32 }
}

fn lat_to_tile_y(lat: f64, z: u32) -> u32 {
    let lat_rad = lat.to_radians();
    let n = f64::from(1u32 << z);
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    { ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor() as u32 }
}

/// Pick up to 5 evenly spaced zoom levels from the available range.
fn pick_zoom_levels(min_zoom: u8, max_zoom: u8) -> Vec<u32> {
    let min_z = u32::from(min_zoom);
    let max_z = u32::from(max_zoom);
    if max_z <= min_z {
        return vec![min_z];
    }
    let range = max_z - min_z;
    let count = range.min(4) + 1; // 2–5 levels
    (0..count)
        .map(|i| min_z + i * range / (count - 1))
        .collect()
}

/// Generate tile coordinates from PMTiles bounds.
///
/// Picks up to 5 zoom levels spread across the archive's range, with a 2×2
/// grid of tiles centered on the bounding box at each level (~20 tiles total).
fn generate_tile_coords(bounds: &pmtiles::TileBounds) -> Vec<(u32, u32, u32)> {
    let center_lon = (bounds.min_lon + bounds.max_lon) / 2.0;
    let center_lat = (bounds.min_lat + bounds.max_lat) / 2.0;
    let zoom_levels = pick_zoom_levels(bounds.min_zoom, bounds.max_zoom);

    let mut coords = Vec::new();
    for z in zoom_levels {
        let max_tile = (1u32 << z).saturating_sub(1);
        let cx = lon_to_tile_x(center_lon, z).min(max_tile);
        let cy = lat_to_tile_y(center_lat, z).min(max_tile);

        coords.push((z, cx, cy));
        if cx < max_tile {
            coords.push((z, cx + 1, cy));
        }
        if cy < max_tile {
            coords.push((z, cx, cy + 1));
        }
        if cx < max_tile && cy < max_tile {
            coords.push((z, cx + 1, cy + 1));
        }
    }
    coords
}

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
#[allow(clippy::too_many_arguments)]
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
    let bounds = pmtiles::read_bounds(Path::new(tiles))?;
    let tile_coords = generate_tile_coords(&bounds);
    output::bench_msg(&format!(
        "z{}-z{}, {} tile coordinates from PMTiles bounds",
        bounds.min_zoom, bounds.max_zoom, tile_coords.len()
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
        run_lifecycle(&binary, &data_dir, &tiles, port, &project_root, &tile_coords)
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
    tile_coords: &[(u32, u32, u32)],
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(min_zoom: u8, max_zoom: u8, min_lon: f64, min_lat: f64, max_lon: f64, max_lat: f64) -> pmtiles::TileBounds {
        pmtiles::TileBounds { min_zoom, max_zoom, min_lon, min_lat, max_lon, max_lat }
    }

    #[test]
    fn pick_zoom_levels_single() {
        assert_eq!(pick_zoom_levels(5, 5), vec![5]);
    }

    #[test]
    fn pick_zoom_levels_small_range() {
        assert_eq!(pick_zoom_levels(10, 12), vec![10, 11, 12]);
    }

    #[test]
    fn pick_zoom_levels_full_range() {
        let levels = pick_zoom_levels(0, 14);
        assert_eq!(levels.len(), 5);
        assert_eq!(levels[0], 0);
        assert_eq!(*levels.last().unwrap(), 14);
    }

    #[test]
    fn copenhagen_tiles_hit_expected_region() {
        // Denmark bbox from brokkr.toml: 12.4,55.6,12.7,55.8
        let b = bounds(0, 14, 12.4, 55.6, 12.7, 55.8);
        let coords = generate_tile_coords(&b);
        assert!(!coords.is_empty());

        // Should have tiles at z14 (the max zoom)
        let z14: Vec<_> = coords.iter().filter(|(z, _, _)| *z == 14).collect();
        assert!(!z14.is_empty(), "should have z14 tiles");

        // All tiles should be in valid range for their zoom
        for &(z, x, y) in &coords {
            let max = (1u32 << z) - 1;
            assert!(x <= max, "x={x} out of range at z{z}");
            assert!(y <= max, "y={y} out of range at z{z}");
        }
    }

    #[test]
    fn generates_roughly_twenty_tiles() {
        // Typical z0-z14 archive
        let b = bounds(0, 14, 5.0, 47.0, 15.0, 55.0);
        let coords = generate_tile_coords(&b);
        // 5 zoom levels × up to 4 tiles = up to 20
        assert!(coords.len() >= 5, "at least one tile per zoom level");
        assert!(coords.len() <= 20, "at most 4 tiles per zoom level × 5 levels");
    }

    #[test]
    fn z0_produces_single_tile() {
        let b = bounds(0, 0, -180.0, -85.0, 180.0, 85.0);
        let coords = generate_tile_coords(&b);
        // z0 has only tile (0,0), so 2×2 grid collapses to 1
        assert_eq!(coords.len(), 1);
        assert_eq!(coords[0], (0, 0, 0));
    }

    #[test]
    fn western_hemisphere_tiles() {
        // New York area
        let b = bounds(0, 14, -74.3, 40.5, -73.7, 40.9);
        let coords = generate_tile_coords(&b);
        assert!(!coords.is_empty());
        for &(z, x, y) in &coords {
            let max = (1u32 << z) - 1;
            assert!(x <= max, "x={x} out of range at z{z}");
            assert!(y <= max, "y={y} out of range at z{z}");
        }
    }
}
