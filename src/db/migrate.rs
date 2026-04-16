use crate::error::DevError;

/// Current schema version. Increment when adding new migrations.
pub(super) const SCHEMA_VERSION: i64 = 15;

/// Run all pending migrations based on `PRAGMA user_version`.
pub(super) fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Fresh database — no tables yet, nothing to migrate.
    if !has_table(conn, "runs") {
        return Ok(());
    }

    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    if current < 1 {
        migrate_uuid(conn)?;
    }
    if current < 2 {
        migrate_cli_args_metadata(conn)?;
    }
    if current < 3 {
        migrate_v2_to_v3(conn)?;
    }
    if current < 4 {
        migrate_v3_to_v4(conn)?;
    }
    if current < 5 {
        migrate_v4_to_v5(conn)?;
    }
    if current < 6 {
        migrate_v5_to_v6(conn)?;
    }
    if current < 7 {
        migrate_v6_to_v7(conn)?;
    }
    if current < 8 {
        migrate_v7_to_v8(conn)?;
    }
    if current < 9 {
        migrate_v8_to_v9(conn)?;
    }
    if current < 10 {
        migrate_v9_to_v10(conn)?;
    }

    if current < 11 {
        migrate_v10_to_v11(conn)?;
    }
    if current < 12 {
        migrate_v11_to_v12(conn)?;
    }
    if current < 13 {
        migrate_v12_to_v13(conn)?;
    }
    if current < 14 {
        migrate_v13_to_v14(conn)?;
    }
    if current < 15 {
        migrate_v14_to_v15(conn)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Check whether a table exists in the database.
fn has_table(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

/// Check whether a column exists on a table.
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .collect();
            Ok(names.contains(&column.to_owned()))
        })
        .unwrap_or(false)
}

/// Migration v0 -> v1: add uuid column and backfill.
fn migrate_uuid(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "uuid") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN uuid TEXT")?;
    }

    // Backfill existing rows with generated UUIDs.
    let mut stmt = conn.prepare("SELECT id FROM runs WHERE uuid IS NULL")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(Result::ok)
        .collect();

    let mut update = conn.prepare("UPDATE runs SET uuid = ?1 WHERE id = ?2")?;
    for id in ids {
        let uuid = super::types::generate_uuid()?;
        update.execute(rusqlite::params![uuid, id])?;
    }

    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_runs_uuid ON runs(uuid)")?;
    Ok(())
}

/// Migration v1 -> v2: add cli_args and metadata columns.
fn migrate_cli_args_metadata(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "cli_args") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN cli_args TEXT")?;
    }
    if !has_column(conn, "runs", "metadata") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN metadata TEXT")?;
    }
    Ok(())
}

/// Migration v2 -> v3: add peak_rss_mb, project columns, create child tables,
/// migrate extra/metadata JSON into child tables.
fn migrate_v2_to_v3(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Phase 1: DDL additions.
    if !has_column(conn, "runs", "peak_rss_mb") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN peak_rss_mb REAL")?;
    }
    if !has_column(conn, "runs", "project") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN project TEXT NOT NULL DEFAULT 'pbfhogg'")?;
    }

    // Child tables + indexes (all idempotent via IF NOT EXISTS).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS run_distribution (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            samples INTEGER NOT NULL, min_ms INTEGER NOT NULL,
            p50_ms INTEGER NOT NULL, p95_ms INTEGER NOT NULL, max_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id));

        CREATE TABLE IF NOT EXISTS run_kv (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            key TEXT NOT NULL, value_int INTEGER, value_real REAL, value_text TEXT,
            PRIMARY KEY (run_id, key));
        CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);

        CREATE TABLE IF NOT EXISTS hotpath_functions (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            section TEXT NOT NULL, description TEXT, ordinal INTEGER NOT NULL,
            name TEXT NOT NULL, calls INTEGER, avg TEXT, total TEXT,
            percent_total TEXT, p50 TEXT, p95 TEXT, p99 TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);

        CREATE TABLE IF NOT EXISTS hotpath_threads (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL, status TEXT, cpu_percent TEXT, cpu_percent_max TEXT,
            cpu_user TEXT, cpu_sys TEXT, cpu_total TEXT,
            alloc_bytes TEXT, dealloc_bytes TEXT, mem_diff TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);

        CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);",
    )?;

    // Phase 2: Migrate existing extra/metadata JSON to child tables.
    migrate_json_to_children(conn)?;

    Ok(())
}

/// Migration v3 -> v4: rename pbfhogg variant values after CLI consolidation (22→14).
///
/// Only touches rows where `project = 'pbfhogg'` (or project IS NULL, which defaults
/// to pbfhogg in older schemas).
fn migrate_v3_to_v4(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Variant renames in bench commands / bench blob-filter / hotpath.
    const RENAMES: &[(&str, &str)] = &[
        ("tags-count", "inspect-tags"),
        ("tags-count-way", "inspect-tags-way"),
        ("node-stats", "inspect-nodes"),
        ("removeid", "getid-invert"),
        ("merge-pbf", "cat-dedupe"),
        ("derive-changes", "diff-osc"),
        // blob-filter compound variants
        ("tags-count-way+indexed", "inspect-tags-way+indexed"),
        ("tags-count-way+raw", "inspect-tags-way+raw"),
        ("node-stats+indexed", "inspect-nodes+indexed"),
        ("node-stats+raw", "inspect-nodes+raw"),
        // hotpath variants
        ("merge-zlib", "apply-changes-zlib"),
        ("merge-none", "apply-changes-none"),
        ("merge-zlib/alloc", "apply-changes-zlib/alloc"),
        ("merge-none/alloc", "apply-changes-none/alloc"),
        ("tags-count/alloc", "inspect-tags/alloc"),
    ];

    let mut stmt = conn.prepare(
        "UPDATE runs SET variant = ?1 WHERE variant = ?2 AND (project = 'pbfhogg' OR project IS NULL)"
    )?;

    for &(old, new) in RENAMES {
        stmt.execute(rusqlite::params![new, old])?;
    }

    Ok(())
}

/// Migration v4 -> v5: rename `meta.locations_on_ways` to `meta.locations_on_ways_cli`
/// in run_kv. Existing rows recorded only CLI intent, not runtime detection.
fn migrate_v4_to_v5(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "run_kv") {
        conn.execute(
            "UPDATE run_kv SET key = 'meta.locations_on_ways_cli' WHERE key = 'meta.locations_on_ways'",
            [],
        )?;
    }
    Ok(())
}

/// Migration v5 -> v6: rename `meta.tiles_sha256` to `meta.tiles_hash`
/// in run_kv after switching from SHA256 to XXH128.
fn migrate_v5_to_v6(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "run_kv") {
        conn.execute(
            "UPDATE run_kv SET key = 'meta.tiles_hash' WHERE key = 'meta.tiles_sha256'",
            [],
        )?;
    }
    Ok(())
}

/// Migration v6 -> v7: replace `cpu_user`, `cpu_sys`, `cpu_total` columns in
/// `hotpath_threads` with `cpu_percent_avg` (hotpath 0.14 schema change).
fn migrate_v6_to_v7(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "hotpath_threads") {
        return Ok(());
    }
    // SQLite doesn't support DROP COLUMN before 3.35.0, and even then it's
    // finicky with constraints.  Safest approach: recreate the table.
    conn.execute_batch(
        "CREATE TABLE hotpath_threads_new (
            id              INTEGER PRIMARY KEY,
            run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name            TEXT NOT NULL,
            status          TEXT,
            cpu_percent     TEXT,
            cpu_percent_max TEXT,
            cpu_percent_avg TEXT,
            alloc_bytes     TEXT,
            dealloc_bytes   TEXT,
            mem_diff        TEXT
        );
        INSERT INTO hotpath_threads_new (id, run_id, name, status, cpu_percent, cpu_percent_max,
            alloc_bytes, dealloc_bytes, mem_diff)
            SELECT id, run_id, name, status, cpu_percent, cpu_percent_max,
                   alloc_bytes, dealloc_bytes, mem_diff
            FROM hotpath_threads;
        DROP TABLE hotpath_threads;
        ALTER TABLE hotpath_threads_new RENAME TO hotpath_threads;
        CREATE INDEX idx_hotpath_threads_run_id ON hotpath_threads(run_id);",
    )?;
    Ok(())
}

/// Migration v7 -> v8: add sidecar profiler tables.
///
/// Fresh databases already have these tables from the SCHEMA DDL, so this
/// migration only runs on databases created before the sidecar feature.
fn migrate_v7_to_v8(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "sidecar_samples") {
        conn.execute_batch(
            "CREATE TABLE sidecar_samples (
                result_uuid TEXT NOT NULL,
                run_idx     INTEGER NOT NULL DEFAULT 0,
                sample_idx  INTEGER NOT NULL,
                timestamp_us INTEGER NOT NULL,
                rss_kb      INTEGER,
                anon_kb     INTEGER,
                file_kb     INTEGER,
                shmem_kb    INTEGER,
                swap_kb     INTEGER,
                vsize_kb    INTEGER,
                vm_hwm_kb   INTEGER,
                utime       INTEGER,
                stime       INTEGER,
                num_threads INTEGER,
                minflt      INTEGER,
                majflt      INTEGER,
                rchar       INTEGER,
                wchar       INTEGER,
                read_bytes  INTEGER,
                write_bytes INTEGER,
                cancelled_write_bytes INTEGER,
                syscr       INTEGER,
                syscw       INTEGER,
                vol_cs      INTEGER,
                nonvol_cs   INTEGER,
                PRIMARY KEY (result_uuid, run_idx, sample_idx)
            );
            CREATE TABLE sidecar_markers (
                result_uuid  TEXT NOT NULL,
                run_idx      INTEGER NOT NULL DEFAULT 0,
                marker_idx   INTEGER NOT NULL,
                timestamp_us INTEGER NOT NULL,
                name         TEXT NOT NULL,
                PRIMARY KEY (result_uuid, run_idx, marker_idx)
            );
            CREATE TABLE sidecar_summary (
                result_uuid  TEXT NOT NULL,
                run_idx      INTEGER NOT NULL DEFAULT 0,
                vm_hwm_kb    INTEGER,
                sample_count INTEGER,
                marker_count INTEGER,
                wall_time_ms INTEGER,
                PRIMARY KEY (result_uuid, run_idx)
            );",
        )?;
    }
    Ok(())
}

/// Migration v8 -> v9: drop sidecar tables from results.db.
///
/// Sidecar data now lives in a separate `.brokkr/sidecar.db` (gitignored)
/// to keep the tracked results.db small.
fn migrate_v8_to_v9(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if has_table(conn, "sidecar_samples") {
        conn.execute_batch(
            "DROP TABLE IF EXISTS sidecar_samples;
             DROP TABLE IF EXISTS sidecar_markers;
             DROP TABLE IF EXISTS sidecar_summary;",
        )?;
    }
    Ok(())
}

/// v9 → v10: add stop_marker column.
fn migrate_v9_to_v10(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "stop_marker") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN stop_marker TEXT")?;
    }
    Ok(())
}

/// v10 → v11: collapse `renumber+external` variant to `renumber`.
///
/// pbfhogg removed the `--mode` flag from `renumber` (external is now the only
/// implementation), so the `+external` variant suffix is no longer produced.
/// Merge old `+external` rows into the plain `renumber` variant for consistent
/// `brokkr results --command renumber` queries.
fn migrate_v10_to_v11(conn: &rusqlite::Connection) -> Result<(), DevError> {
    conn.execute(
        "UPDATE runs SET variant = 'renumber' WHERE variant = 'renumber+external'",
        [],
    )?;
    Ok(())
}

/// v11 → v12: move the real command name out of `variant` and into `command`.
///
/// Historically every pbfhogg tool-CLI command (inspect, sort, cat, diff,
/// add-locations-to-ways, ...) stored `command = 'bench commands'` with the
/// real name stuffed into `variant` (optionally trailed with `+<suffix>`
/// like `+nocompress`, `+direct-io`, `+range-LO-HI`). Hotpath rows had the
/// same shape with `command = 'hotpath'` and an extra `/alloc` marker for
/// alloc-tracking runs.
///
/// That made the `command` column useless and the `variant` column a
/// name-plus-axis jumble. After this migration the command column carries
/// the real name (`'bench add-locations-to-ways'`, `'hotpath inspect'`)
/// and the variant column holds only real variance axes (`'nocompress'`,
/// `'direct-io+zstd1'`, `'range-4914-4920'`, `'alloc'`).
///
/// Only pbfhogg rows are touched — the elivagar/nidhogg/sluggrs `command`
/// values were never affected by the legacy naming.
fn migrate_v11_to_v12(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Idempotency guard. A DB can legitimately reach this migration with
    // `variant` already gone if a different brokkr branch applied a
    // superset of these steps and didn't bump `user_version` past 11
    // (observed on databases copied between projects). Treat it as
    // already-applied rather than erroring.
    if !has_column(conn, "runs", "variant") {
        return Ok(());
    }

    // --- bench rows --------------------------------------------------------
    //
    // Normalize diff-snapshots variants (legacy `-from-to-to` suffix) into
    // the standard `+` separator shape so the split-on-+ step handles them
    // uniformly.
    conn.execute(
        "UPDATE runs \
         SET variant = 'diff-snapshots+' || substr(variant, length('diff-snapshots-') + 1) \
         WHERE command = 'bench commands' \
           AND variant LIKE 'diff-snapshots-%'",
        [],
    )?;

    // Rows with no `+` suffix: the whole variant is the command name.
    conn.execute(
        "UPDATE runs \
         SET command = 'bench ' || variant, variant = NULL \
         WHERE command = 'bench commands' \
           AND variant IS NOT NULL \
           AND variant NOT LIKE '%+%'",
        [],
    )?;

    // Rows with a `+` suffix: split on the first `+`. `variant[..first_plus]`
    // becomes the command suffix, `variant[first_plus+1..]` becomes the new
    // variant (NULL if empty).
    conn.execute(
        "UPDATE runs \
         SET command = 'bench ' || substr(variant, 1, instr(variant, '+') - 1), \
             variant = NULLIF(substr(variant, instr(variant, '+') + 1), '') \
         WHERE command = 'bench commands' \
           AND variant LIKE '%+%'",
        [],
    )?;

    // --- hotpath rows ------------------------------------------------------
    //
    // Variant formats in the wild: `<id>`, `<id>/alloc`, `<id>+<suffix>`,
    // `<id>/alloc+<suffix>`. Split on whichever separator (`/` or `+`)
    // appears first. The separator itself is dropped — except the `/alloc`
    // marker, whose content is kept in the new variant as `alloc`.
    //
    // Restricted to pbfhogg so elivagar/nidhogg/sluggrs hotpath rows are
    // left untouched.

    // A. '/' appears and (no '+' OR '/' is earlier than '+'): split on '/'.
    //    Everything after the '/' (e.g. `alloc+direct-io`) becomes variant.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || substr(variant, 1, instr(variant, '/') - 1), \
             variant = NULLIF(substr(variant, instr(variant, '/') + 1), '') \
         WHERE command = 'hotpath' \
           AND project = 'pbfhogg' \
           AND variant IS NOT NULL \
           AND instr(variant, '/') > 0 \
           AND (instr(variant, '+') = 0 OR instr(variant, '/') < instr(variant, '+'))",
        [],
    )?;

    // B. '+' appears and (no '/' OR '+' is earlier than '/'): split on '+'.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || substr(variant, 1, instr(variant, '+') - 1), \
             variant = NULLIF(substr(variant, instr(variant, '+') + 1), '') \
         WHERE command = 'hotpath' \
           AND project = 'pbfhogg' \
           AND variant IS NOT NULL \
           AND instr(variant, '+') > 0 \
           AND (instr(variant, '/') = 0 OR instr(variant, '+') < instr(variant, '/'))",
        [],
    )?;

    // C. No separator: the whole variant is the command id.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || variant, variant = NULL \
         WHERE command = 'hotpath' \
           AND project = 'pbfhogg' \
           AND variant IS NOT NULL \
           AND instr(variant, '/') = 0 \
           AND instr(variant, '+') = 0",
        [],
    )?;

    Ok(())
}

/// v12 → v13: `command` becomes the bare pbfhogg/elivagar/nidhogg
/// subcommand name, `variant` becomes the measurement mode
/// (`bench`/`hotpath`/`alloc`), and runtime-redundant `meta.*` keys
/// (axes already captured in `cli_args` or the new `brokkr_args`) are
/// dropped. Adds the `brokkr_args TEXT` column for future writes; all
/// historical rows leave it NULL.
///
/// Steps:
///   1. Add the `brokkr_args` column.
///   2. Extend v11→v12's hotpath split to *all* projects (the previous
///      migration restricted it to pbfhogg; elivagar/nidhogg/sluggrs
///      hotpath rows still have `command = 'hotpath'` with the id jammed
///      into variant, and a `/alloc` marker).
///   3. Strip the `bench `/`hotpath ` prefix from command and set variant
///      to the measurement mode. Old variant content (axis suffixes like
///      `+nocompress`, `+direct-io`, etc.) drops — it was always a mirror
///      of cli_args, nothing is lost.
///   4. Delete redundant `meta.*` keys from run_kv. run_kv survives only
///      for genuine runtime observations (cache state, detected features,
///      resolved file info).
fn migrate_v12_to_v13(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // 1. Add brokkr_args column (if not already present — guards against a
    //    fresh SCHEMA that already has it).
    if !has_column(conn, "runs", "brokkr_args") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN brokkr_args TEXT")?;
    }

    // Idempotency guard for the rest: every subsequent step reads/writes
    // `variant`. If it's already been renamed to `mode` (post-v14) by a
    // different brokkr branch that didn't bump user_version, skip.
    if !has_column(conn, "runs", "variant") {
        return Ok(());
    }

    // 2. Extend v11→v12's hotpath split to non-pbfhogg projects. Same SQL
    //    pattern, `project = 'pbfhogg'` filter dropped.

    // 2A. '/' appears and (no '+' OR '/' is earlier than '+'): split on '/'.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || substr(variant, 1, instr(variant, '/') - 1), \
             variant = NULLIF(substr(variant, instr(variant, '/') + 1), '') \
         WHERE command = 'hotpath' \
           AND variant IS NOT NULL \
           AND instr(variant, '/') > 0 \
           AND (instr(variant, '+') = 0 OR instr(variant, '/') < instr(variant, '+'))",
        [],
    )?;

    // 2B. '+' appears and (no '/' OR '+' is earlier than '/'): split on '+'.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || substr(variant, 1, instr(variant, '+') - 1), \
             variant = NULLIF(substr(variant, instr(variant, '+') + 1), '') \
         WHERE command = 'hotpath' \
           AND variant IS NOT NULL \
           AND instr(variant, '+') > 0 \
           AND (instr(variant, '/') = 0 OR instr(variant, '+') < instr(variant, '/'))",
        [],
    )?;

    // 2C. No separator: the whole variant is the command id.
    conn.execute(
        "UPDATE runs \
         SET command = 'hotpath ' || variant, variant = NULL \
         WHERE command = 'hotpath' \
           AND variant IS NOT NULL \
           AND instr(variant, '/') = 0 \
           AND instr(variant, '+') = 0",
        [],
    )?;

    // 3. Strip prefix + set variant to mode.
    //    - `bench X` → command `X`, variant `bench`
    //    - `hotpath X` AND variant LIKE 'alloc%' → command `X`, variant `alloc`
    //    - `hotpath X` (otherwise) → command `X`, variant `hotpath`

    conn.execute(
        "UPDATE runs \
         SET command = substr(command, length('bench ') + 1), \
             variant = 'bench' \
         WHERE command LIKE 'bench %'",
        [],
    )?;

    // Hotpath alloc rows first (variant starts with `alloc`), so the
    // subsequent hotpath-non-alloc pass doesn't overwrite them.
    conn.execute(
        "UPDATE runs \
         SET command = substr(command, length('hotpath ') + 1), \
             variant = 'alloc' \
         WHERE command LIKE 'hotpath %' \
           AND variant IS NOT NULL \
           AND (variant = 'alloc' OR variant LIKE 'alloc+%' OR variant LIKE 'alloc/%')",
        [],
    )?;

    conn.execute(
        "UPDATE runs \
         SET command = substr(command, length('hotpath ') + 1), \
             variant = 'hotpath' \
         WHERE command LIKE 'hotpath %'",
        [],
    )?;

    // 4. Delete redundant meta.* keys. These mirror data already in
    //    cli_args (subprocess flags) or brokkr_args (the brokkr
    //    invocation, NULL for historical rows but grepable for future
    //    rows). Keys not in this list are kept — they represent genuine
    //    runtime observations.
    //
    //    Guard on `run_kv` existence: old-schema migration tests (v3,
    //    v0→v3) start from a schema that only contains the `runs` table
    //    and rely on `ResultsDb::open` to call `SCHEMA` *after* this
    //    migration to create the missing child tables.
    if has_table(conn, "run_kv") {
        conn.execute(
            "DELETE FROM run_kv WHERE key IN ( \
                'meta.compression', 'meta.writer_mode', 'meta.io_mode', 'meta.mode', \
                'meta.strategy', 'meta.bbox', 'meta.regions', \
                'meta.snapshot', 'meta.from_snapshot', 'meta.to_snapshot', \
                'meta.index_type', 'meta.start_stage', 'meta.keep_scratch', \
                'meta.format', 'meta.alloc', 'meta.test', \
                'meta.skip_to', 'meta.compression_level', \
                'meta.ocean', 'meta.force_sorted', 'meta.allow_unsafe_flat_index', \
                'meta.tile_format', 'meta.tile_compression', 'meta.compress_sort_chunks', \
                'meta.in_memory', 'meta.locations_on_ways_cli', \
                'meta.fanout_cap_default', 'meta.fanout_cap', 'meta.polygon_simplify_factor', \
                'meta.query', 'meta.uring' \
             )",
            [],
        )?;
    }

    Ok(())
}

/// v13 → v14: rename the `variant` column to `mode`.
///
/// After v13, the `variant` column holds nothing but the measurement
/// mode (`bench`/`hotpath`/`alloc`) — the name was inherited from when
/// it carried a freeform axis bag. Renaming to `mode` makes the
/// semantics obvious and lets future docs/filters read naturally.
///
/// Uses SQLite's `ALTER TABLE ... RENAME COLUMN` (available since
/// SQLite 3.25). Indexes on the column (if any) and child-table
/// foreign keys (none — variant was never referenced) are unaffected.
fn migrate_v13_to_v14(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Guard: only rename if the old column still exists. On fresh
    // databases `SCHEMA` already created the table with `mode`, so the
    // rename would fail.
    if has_column(conn, "runs", "variant") && !has_column(conn, "runs", "mode") {
        conn.execute_batch("ALTER TABLE runs RENAME COLUMN variant TO mode")?;
    }
    Ok(())
}

/// v14 → v15: collapse preset subcommand names into their consolidated
/// forms after the cat/extract/tags-filter/getid/inspect/diff
/// consolidation.
///
/// Historical rows carry the old preset spelling in the `command`
/// column (e.g. `cat-way`, `extract-simple`, `tags-filter-amenity`,
/// `getid-refs`, `inspect-tags-way`, `diff-osc`). The consolidated
/// subcommands live under a single name per family (`cat`, `extract`,
/// `tags-filter`, `getid`, `inspect`, `diff`); the distinguishing
/// flags are already present in each row's `cli_args`, so no flag
/// reconstruction is needed — we just rewrite the command column.
fn migrate_v14_to_v15(conn: &rusqlite::Connection) -> Result<(), DevError> {
    const RENAMES: &[(&str, &str)] = &[
        // Cat family.
        ("cat-way", "cat"),
        ("cat-relation", "cat"),
        ("cat-dedupe", "cat"),
        ("cat-clean", "cat"),
        // Extract family.
        ("extract-simple", "extract"),
        ("extract-complete", "extract"),
        ("extract-smart", "extract"),
        // Tags-filter family.
        ("tags-filter-way", "tags-filter"),
        ("tags-filter-amenity", "tags-filter"),
        ("tags-filter-twopass", "tags-filter"),
        ("tags-filter-osc", "tags-filter"),
        // Getid family.
        ("getid-refs", "getid"),
        ("getid-invert", "getid"),
        // Inspect family.
        ("inspect-nodes", "inspect"),
        ("inspect-tags", "inspect"),
        ("inspect-tags-way", "inspect"),
        // Diff family.
        ("diff-osc", "diff"),
    ];

    let mut stmt = conn.prepare("UPDATE runs SET command = ?1 WHERE command = ?2")?;
    for &(old, new) in RENAMES {
        stmt.execute(rusqlite::params![new, old])?;
    }

    Ok(())
}

/// Parse existing extra/metadata JSON and insert into child tables.
fn migrate_json_to_children(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT id, extra, metadata FROM runs WHERE extra IS NOT NULL OR metadata IS NOT NULL",
    )?;
    let rows: Vec<(i64, Option<String>, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(Result::ok)
        .collect();

    for (run_id, extra_json, metadata_json) in &rows {
        // Migrate extra JSON.
        if let Some(json_str) = extra_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            migrate_extra_object(conn, *run_id, obj)?;
        }
        // Migrate metadata JSON -> run_kv with meta. prefix.
        if let Some(json_str) = metadata_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            for (key, value) in obj {
                let prefixed = format!("meta.{key}");
                insert_kv_from_json(conn, *run_id, &prefixed, value)?;
            }
        }
    }

    Ok(())
}

/// Migrate a single extra JSON object into the appropriate child tables.
fn migrate_extra_object(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DevError> {
    // Case 1: Distribution stats.
    let is_distribution = obj.contains_key("min_ms")
        && obj.contains_key("p50_ms")
        && obj.contains_key("p95_ms")
        && obj.contains_key("max_ms")
        && obj.contains_key("samples");

    if is_distribution {
        let get_i64 = |k: &str| obj.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        conn.execute(
            "INSERT OR IGNORE INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![run_id, get_i64("samples"), get_i64("min_ms"), get_i64("p50_ms"), get_i64("p95_ms"), get_i64("max_ms")],
        )?;
        // Remaining keys (beyond the 5 distribution keys) go to run_kv.
        let dist_keys = ["min_ms", "p50_ms", "p95_ms", "max_ms", "samples"];
        for (key, value) in obj {
            if !dist_keys.contains(&key.as_str()) {
                insert_kv_from_json(conn, run_id, key, value)?;
            }
        }
        return Ok(());
    }

    // Case 2: Hotpath data.
    let is_hotpath = obj.contains_key("functions_timing") || obj.contains_key("functions_alloc");

    if is_hotpath {
        migrate_hotpath_section(conn, run_id, obj, "functions_timing", "timing")?;
        migrate_hotpath_section(conn, run_id, obj, "functions_alloc", "alloc")?;

        if let Some(threads_val) = obj.get("threads")
            && let Some(threads_obj) = threads_val.as_object()
        {
            // Thread summary stats -> run_kv with threads. prefix.
            for key in &[
                "rss_bytes",
                "total_alloc_bytes",
                "total_dealloc_bytes",
                "alloc_dealloc_diff",
            ] {
                if let Some(v) = threads_obj.get(*key) {
                    let prefixed = format!("threads.{key}");
                    insert_kv_from_json(conn, run_id, &prefixed, v)?;
                }
            }
            // Thread data rows.
            if let Some(data) = threads_obj.get("data").and_then(|v| v.as_array()) {
                for entry in data {
                    let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
                    conn.execute(
                        "INSERT INTO hotpath_threads \
                         (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, \
                          alloc_bytes, dealloc_bytes, mem_diff) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        rusqlite::params![
                            run_id,
                            s("name").unwrap_or_default(),
                            s("status"), s("cpu_percent"), s("cpu_percent_max"),
                            s("cpu_user"), s("cpu_sys"), s("cpu_total"),
                            s("alloc_bytes"), s("dealloc_bytes"), s("mem_diff"),
                        ],
                    )?;
                }
            }
        }
        return Ok(());
    }

    // Case 3: Plain kv pairs.
    for (key, value) in obj {
        insert_kv_from_json(conn, run_id, key, value)?;
    }

    Ok(())
}

/// Migrate a hotpath functions section (timing or alloc) from JSON to the child table.
fn migrate_hotpath_section(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
    json_key: &str,
    section_name: &str,
) -> Result<(), DevError> {
    let Some(section_val) = obj.get(json_key) else {
        return Ok(());
    };
    let Some(section_obj) = section_val.as_object() else {
        return Ok(());
    };
    let description = section_obj.get("description").and_then(|v| v.as_str());
    let Some(data) = section_obj.get("data").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    for (ordinal, entry) in data.iter().enumerate() {
        let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
        let calls = entry.get("calls").and_then(serde_json::Value::as_i64);
        #[allow(clippy::cast_possible_wrap)]
        let ord = ordinal as i64;
        conn.execute(
            "INSERT INTO hotpath_functions \
             (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id, section_name, description,
                ord,
                s("name").unwrap_or_default(),
                calls, s("avg"), s("total"), s("percent_total"),
                s("p50"), s("p95"), s("p99"),
            ],
        )?;
    }

    Ok(())
}

/// Insert a single JSON value into run_kv, auto-detecting type.
fn insert_kv_from_json(
    conn: &rusqlite::Connection,
    run_id: i64,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), DevError> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            } else if let Some(v) = n.as_f64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            }
        }
        serde_json::Value::String(s) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, s],
            )?;
        }
        serde_json::Value::Bool(b) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, b.to_string()],
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{QueryFilter, ResultsDb};

    // -----------------------------------------------------------------------
    // Old schema definitions for migration tests
    // -----------------------------------------------------------------------

    /// v0 schema: no uuid, cli_args, metadata, peak_rss_mb, or project columns.
    const V0_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT
        )";

    /// v2 schema: adds uuid, cli_args, metadata over v0.
    const V2_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT, uuid TEXT, cli_args TEXT, metadata TEXT
        )";

    const V0_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1)";

    const V2_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra, uuid, cli_args, metadata)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1, 'existing_uuid', '--fast', ?2)";

    /// Create a temp directory and db path with a unique name per test.
    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brokkr_test_{name}"));
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));
        (dir, db_path)
    }

    fn cleanup(dir: &std::path::Path, db_path: &std::path::Path) {
        drop(std::fs::remove_file(db_path));
        // WAL/SHM files.
        drop(std::fs::remove_file(db_path.with_extension("db-wal")));
        drop(std::fs::remove_file(db_path.with_extension("db-shm")));
        drop(std::fs::remove_dir(dir));
    }

    // -----------------------------------------------------------------------
    // Migration: v0 -> v3
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v0_to_v3() {
        let (dir, db_path) = test_db("migrate_v0");

        // Create v0 database with one row.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V0_SCHEMA).unwrap();
            conn.execute(V0_INSERT, rusqlite::params![rusqlite::types::Null])
                .unwrap();
        }

        // Open via ResultsDb — triggers all migrations.
        let db = ResultsDb::open(&db_path).expect("open should migrate v0 to v3");

        // Row is preserved and queryable.
        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabb")),
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        // v12→v13 strips the `bench ` prefix; the original row was
        // `("bench read", "mmap")` and lands as command=`read`,
        // mode=`bench` here (after v13→v14 renamed `variant`→`mode`).
        assert_eq!(rows[0].command, "read");
        assert_eq!(rows[0].mode, "bench");
        assert_eq!(rows[0].elapsed_ms, 1234);

        // UUID was backfilled.
        assert!(!rows[0].uuid.is_empty(), "uuid should be backfilled");

        // project defaults to pbfhogg.
        assert_eq!(rows[0].project, "pbfhogg");

        // Schema version is current.
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Child tables exist (can query without error).
        db.conn
            .execute_batch("SELECT COUNT(*) FROM run_distribution")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM run_kv")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM hotpath_functions")
            .unwrap();
        db.conn
            .execute_batch("SELECT COUNT(*) FROM hotpath_threads")
            .unwrap();

        // New columns exist.
        assert!(has_column(&db.conn, "runs", "peak_rss_mb"));
        assert!(has_column(&db.conn, "runs", "project"));
        assert!(has_column(&db.conn, "runs", "cli_args"));
        assert!(has_column(&db.conn, "runs", "metadata"));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with distribution JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_distribution_json() {
        let (dir, db_path) = test_db("migrate_v2_dist");

        let extra = r#"{"samples":10,"min_ms":100,"p50_ms":150,"p95_ms":200,"max_ms":250,"output_bytes":999}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null])
                .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v2 to v3");

        // project column added with default.
        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabb")),
                command: None,
                mode: None,
                dataset: None,
                meta: vec![],
                grep: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, "pbfhogg");

        // Distribution migrated to child table.
        let dist: (i64, i64, i64, i64, i64) = db.conn.query_row(
            "SELECT samples, min_ms, p50_ms, p95_ms, max_ms FROM run_distribution WHERE run_id = 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).expect("distribution row should exist");
        assert_eq!(dist, (10, 100, 150, 200, 250));

        // Extra kv (output_bytes) migrated to run_kv.
        let val: i64 = db
            .conn
            .query_row(
                "SELECT value_int FROM run_kv WHERE run_id = 1 AND key = 'output_bytes'",
                [],
                |r| r.get(0),
            )
            .expect("output_bytes kv should exist");
        assert_eq!(val, 999);

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with hotpath JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_hotpath_json() {
        let (dir, db_path) = test_db("migrate_v2_hotpath");

        let extra = r#"{
            "functions_timing": {
                "description": "wall-clock timing",
                "data": [
                    {"name": "parse_header", "calls": 100, "avg": "1.2ms", "total": "120ms", "percent_total": "60%"},
                    {"name": "parse_body", "calls": 50, "avg": "2.0ms", "total": "100ms", "percent_total": "40%"}
                ]
            },
            "threads": {
                "rss_bytes": "1048576",
                "data": [
                    {"name": "main", "status": "running", "cpu_percent": "95%"}
                ]
            }
        }"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null])
                .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate hotpath");

        // Hotpath functions migrated.
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM hotpath_functions WHERE run_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "should have 2 hotpath function rows");

        // Check first function.
        let (name, calls, section): (String, i64, String) = db.conn.query_row(
            "SELECT name, calls, section FROM hotpath_functions WHERE run_id = 1 AND ordinal = 0",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(name, "parse_header");
        assert_eq!(calls, 100);
        assert_eq!(section, "timing");

        // Thread data migrated.
        let thread_name: String = db
            .conn
            .query_row(
                "SELECT name FROM hotpath_threads WHERE run_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thread_name, "main");

        // Thread summary kv migrated.
        let rss: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'threads.rss_bytes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rss, "1048576");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with metadata JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_metadata_json() {
        let (dir, db_path) = test_db("migrate_v2_meta");

        // Use keys that survive v13's cleanup (runtime observations, not
        // axis mirrors) so we can verify the v2→v3 JSON-to-run_kv
        // migration preserves them end-to-end.
        let metadata = r#"{"merged_cache":"cached","locations_on_ways_detected":"true"}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(
                V2_INSERT,
                rusqlite::params![rusqlite::types::Null, metadata],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate metadata");

        // Metadata migrated to run_kv with meta. prefix.
        let merged_cache: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.merged_cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(merged_cache, "cached");

        let detected: String = db
            .conn
            .query_row(
                "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.locations_on_ways_detected'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(detected, "true");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v3 -> v4 variant renames
    // -----------------------------------------------------------------------

    /// v3 schema (full current schema minus v4 changes).
    const V3_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            peak_rss_mb REAL, cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT, uuid TEXT, cli_args TEXT, metadata TEXT,
            project TEXT NOT NULL DEFAULT 'pbfhogg'
        )";

    const V3_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, uuid, project)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'test', ?1, ?2,
            'denmark.osm.pbf', 42.5, 1234, 'uuid1', ?3)";

    #[test]
    fn migrate_v3_to_v4_renames_variants() {
        let (dir, db_path) = test_db("migrate_v3_v4");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();

            // pbfhogg rows that should be renamed.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "tags-count", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "node-stats", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "removeid", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "derive-changes", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "merge-pbf", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench blob-filter", "node-stats+raw", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "merge-zlib", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "tags-count/alloc", "pbfhogg"],
            )
            .unwrap();

            // pbfhogg row that should NOT be renamed (already correct).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "inspect", "pbfhogg"],
            )
            .unwrap();

            // elivagar row with same old variant name — should NOT be touched.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench self", "tags-count", "elivagar"],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v3 to v4");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Helper to query (command, variant) by row id. These tests start
        // at v3 and run every migration up to the current SCHEMA_VERSION,
        // so assertions reflect the cumulative end-state. v3→v4 renames
        // variants, v11→v12 splits `bench commands` rows, v12→v13 strips
        // `bench `/`hotpath ` prefixes and shrinks variant to the
        // measurement mode.
        let row_of = |id: i64| -> (String, Option<String>) {
            db.conn
                .query_row(
                    "SELECT command, mode FROM runs WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap()
        };

        // Rows 1-5, 9: originally `bench commands` / <variant>.
        //   v3→v4 renames the variant (e.g. tags-count → inspect-tags);
        //   v11→v12 splits into `bench <id>` / NULL;
        //   v12→v13 strips `bench ` and sets variant = 'bench';
        //   v13→v14 renames variant column to mode (no data change);
        //   v14→v15 collapses preset names (inspect-tags → inspect,
        //     getid-invert → getid, diff-osc → diff, cat-dedupe → cat).
        assert_eq!(row_of(1), ("inspect".into(), Some("bench".into())));
        assert_eq!(row_of(2), ("inspect".into(), Some("bench".into())));
        assert_eq!(row_of(3), ("getid".into(), Some("bench".into())));
        assert_eq!(row_of(4), ("diff".into(), Some("bench".into())));
        assert_eq!(row_of(5), ("cat".into(), Some("bench".into())));

        // Row 6: `bench blob-filter` / `node-stats+raw` — v3→v4 renames
        // variant to `inspect-nodes+raw`; v11→v12 skips (command wasn't
        // `bench commands`); v12→v13 strips `bench ` prefix and sets
        // variant to `bench`, dropping the old axis-bag content.
        assert_eq!(row_of(6), ("blob-filter".into(), Some("bench".into())));

        // Rows 7-8: pbfhogg `hotpath` rows. v3→v4 renames the variant;
        // v11→v12 splits `hotpath X` out; v12→v13 strips `hotpath ` and
        // sets variant to `hotpath` or `alloc`. v14→v15 collapses the
        // `inspect-tags` preset on row 8 down to `inspect`.
        assert_eq!(row_of(7), ("apply-changes-zlib".into(), Some("hotpath".into())));
        assert_eq!(row_of(8), ("inspect".into(), Some("alloc".into())));

        // Row 9: `bench commands` / `inspect` → `bench inspect` / NULL →
        // `inspect` / `bench`.
        assert_eq!(row_of(9), ("inspect".into(), Some("bench".into())));

        // Row 10: elivagar `bench self` / `tags-count` — v3→v4 skipped
        // (project filter); v11→v12 skipped (not `bench commands`);
        // v12→v13 strips `bench ` and overwrites variant with `bench`.
        assert_eq!(row_of(10), ("self".into(), Some("bench".into())));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v11 -> v12 splits `bench commands` rows
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v11_to_v12_splits_bench_commands() {
        let (dir, db_path) = test_db("migrate_v11_v12");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 11).unwrap();

            // 1. Bare command (no suffix).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench commands", "inspect", "pbfhogg"],
            )
            .unwrap();
            // 2. Single +suffix (`nocompress`).
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "bench commands",
                    "add-locations-to-ways+nocompress",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // 3. Multiple + suffixes: only the first + splits the command.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "bench commands",
                    "apply-changes+direct-io+zstd1",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // 4. Range suffix.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "bench commands",
                    "merge-changes+range-4914-4920",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // 5. Legacy diff-snapshots with `-from-to-to` trailer (no +):
            //    step 1 of the migration rewrites it to `diff-snapshots+...`
            //    before the split-on-+ pass runs.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "bench commands",
                    "diff-snapshots-base-to-20260411",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // 6. `bench extract` — command already distinct, left alone.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench extract", "simple", "pbfhogg"],
            )
            .unwrap();
            // 7. Non-pbfhogg row — untouched.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench self", "whatever", "elivagar"],
            )
            .unwrap();
            // 8. pbfhogg hotpath, bare id (no alloc, no io flags).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "inspect-tags", "pbfhogg"],
            )
            .unwrap();
            // 9. pbfhogg hotpath with `/alloc` marker only.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "inspect-tags/alloc", "pbfhogg"],
            )
            .unwrap();
            // 10. pbfhogg hotpath with io/extra suffix.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "apply-changes+direct-io", "pbfhogg"],
            )
            .unwrap();
            // 11. pbfhogg hotpath with `/alloc` and io/extra suffix.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "hotpath",
                    "apply-changes/alloc+direct-io+zstd1",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // 12. Non-pbfhogg hotpath row — untouched.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "tilegen/alloc", "elivagar"],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v11 to v12");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let row_of = |id: i64| -> (String, Option<String>) {
            db.conn
                .query_row(
                    "SELECT command, mode FROM runs WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap()
        };

        // End-state after all migrations. v11→v12 splits `bench commands`
        // into `bench <id>`; v13 strips `bench ` and sets mode; v14→v15
        // collapses preset names (inspect-tags → inspect, diff-snapshots
        // is its own distinct command and stays).
        assert_eq!(row_of(1), ("inspect".into(), Some("bench".into())));
        assert_eq!(
            row_of(2),
            ("add-locations-to-ways".into(), Some("bench".into()))
        );
        assert_eq!(row_of(3), ("apply-changes".into(), Some("bench".into())));
        assert_eq!(row_of(4), ("merge-changes".into(), Some("bench".into())));
        assert_eq!(row_of(5), ("diff-snapshots".into(), Some("bench".into())));
        assert_eq!(row_of(6), ("extract".into(), Some("bench".into())));
        assert_eq!(row_of(7), ("self".into(), Some("bench".into())));
        // Hotpath rows. Rows 8-9 were `hotpath inspect-tags` / alloc
        // marker — v14→v15 collapses preset name to `inspect`.
        assert_eq!(row_of(8), ("inspect".into(), Some("hotpath".into())));
        assert_eq!(row_of(9), ("inspect".into(), Some("alloc".into())));
        assert_eq!(
            row_of(10),
            ("apply-changes".into(), Some("hotpath".into()))
        );
        assert_eq!(row_of(11), ("apply-changes".into(), Some("alloc".into())));
        assert_eq!(row_of(12), ("tilegen".into(), Some("alloc".into())));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v12 → v13 — strip command prefix, variant becomes mode,
    // drop redundant meta.* keys, add brokkr_args column.
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v12_to_v13_rewrites_command_and_variant() {
        let (dir, db_path) = test_db("migrate_v12_v13");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            // v12→v13 also deletes redundant `meta.*` keys from run_kv;
            // V3_SCHEMA only creates `runs`, so we have to stand up the
            // child table explicitly for this test to exercise step 4.
            conn.execute_batch(
                "CREATE TABLE run_kv ( \
                    run_id INTEGER NOT NULL, \
                    key TEXT NOT NULL, \
                    value_int INTEGER, value_real REAL, value_text TEXT, \
                    PRIMARY KEY (run_id, key) \
                 )",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 12).unwrap();

            // Post-v12 bench row: `bench <id>` / `<axes>`. After v13:
            // `<id>` / `bench`, with the axes dropped (they live in
            // cli_args / brokkr_args for future rows).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench add-locations-to-ways", "nocompress", "pbfhogg"],
            )
            .unwrap();
            // Post-v12 bench row with NULL variant.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "bench inspect",
                    rusqlite::types::Null,
                    "pbfhogg"
                ],
            )
            .unwrap();
            // Post-v12 pbfhogg hotpath row (non-alloc).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath apply-changes", "direct-io", "pbfhogg"],
            )
            .unwrap();
            // Post-v12 pbfhogg hotpath row with alloc marker.
            conn.execute(
                V3_INSERT,
                rusqlite::params![
                    "hotpath apply-changes",
                    "alloc+direct-io",
                    "pbfhogg"
                ],
            )
            .unwrap();
            // Elivagar hotpath (pre-split — still has the id in variant
            // because v11→v12 was pbfhogg-only). v13 must split it too.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "tilegen/alloc", "elivagar"],
            )
            .unwrap();
            // Elivagar hotpath with only a '+' suffix, no '/alloc'.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["hotpath", "pmtiles+gzip", "elivagar"],
            )
            .unwrap();
            // Elivagar bench row.
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench self", rusqlite::types::Null, "elivagar"],
            )
            .unwrap();
            // Nidhogg bench with existing variant (e.g. api query name).
            conn.execute(
                V3_INSERT,
                rusqlite::params!["bench api", "bbox-small", "nidhogg"],
            )
            .unwrap();

            // Add a meta.* key that should be deleted (duplicates cli_args)
            // and one that should survive (runtime observation).
            // Row id is assigned sequentially starting at 1; we latch onto
            // row 1 for the delete case and row 2 for the keep case.
            conn.execute(
                "INSERT INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![1, "meta.compression", "zstd:1"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![2, "meta.merged_cache", "cached"],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v12 to v13");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // brokkr_args column now exists (NULL for historical rows).
        assert!(has_column(&db.conn, "runs", "brokkr_args"));

        let row_of = |id: i64| -> (String, Option<String>) {
            db.conn
                .query_row(
                    "SELECT command, mode FROM runs WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .unwrap()
        };

        assert_eq!(
            row_of(1),
            ("add-locations-to-ways".into(), Some("bench".into()))
        );
        assert_eq!(row_of(2), ("inspect".into(), Some("bench".into())));
        assert_eq!(
            row_of(3),
            ("apply-changes".into(), Some("hotpath".into()))
        );
        assert_eq!(row_of(4), ("apply-changes".into(), Some("alloc".into())));
        assert_eq!(row_of(5), ("tilegen".into(), Some("alloc".into())));
        assert_eq!(row_of(6), ("pmtiles".into(), Some("hotpath".into())));
        assert_eq!(row_of(7), ("self".into(), Some("bench".into())));
        assert_eq!(row_of(8), ("api".into(), Some("bench".into())));

        // run_kv: meta.compression deleted, meta.merged_cache kept.
        let deleted: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_kv WHERE key = 'meta.compression'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 0, "meta.compression should have been deleted");
        let kept: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_kv WHERE key = 'meta.merged_cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "meta.merged_cache should be preserved");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v13 → v14 renames `variant` column to `mode`
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v13_to_v14_renames_variant_to_mode() {
        let (dir, db_path) = test_db("migrate_v13_v14");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.execute_batch(
                "CREATE TABLE run_kv ( \
                    run_id INTEGER NOT NULL, \
                    key TEXT NOT NULL, \
                    value_int INTEGER, value_real REAL, value_text TEXT, \
                    PRIMARY KEY (run_id, key) \
                 )",
            )
            .unwrap();
            // Post-v13 row shape: command = bare id, variant = measurement
            // mode (this is what v13 produces and v14 inherits).
            conn.execute_batch("ALTER TABLE runs ADD COLUMN brokkr_args TEXT").unwrap();
            conn.pragma_update(None, "user_version", 13).unwrap();

            conn.execute(
                V3_INSERT,
                rusqlite::params!["add-locations-to-ways", "bench", "pbfhogg"],
            )
            .unwrap();
            conn.execute(
                V3_INSERT,
                rusqlite::params!["inspect-tags", "hotpath", "pbfhogg"],
            )
            .unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v13 to v14");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // `variant` gone, `mode` present and carrying the same values.
        assert!(!has_column(&db.conn, "runs", "variant"));
        assert!(has_column(&db.conn, "runs", "mode"));

        let mode_of = |id: i64| -> Option<String> {
            db.conn
                .query_row("SELECT mode FROM runs WHERE id = ?1", [id], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .unwrap()
        };
        assert_eq!(mode_of(1), Some("bench".into()));
        assert_eq!(mode_of(2), Some("hotpath".into()));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v14 → v15 collapses preset command names into consolidated
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v14_to_v15_rewrites_preset_command_names() {
        let (dir, db_path) = test_db("migrate_v14_v15");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V3_SCHEMA).unwrap();
            conn.execute_batch(
                "CREATE TABLE run_kv ( \
                    run_id INTEGER NOT NULL, \
                    key TEXT NOT NULL, \
                    value_int INTEGER, value_real REAL, value_text TEXT, \
                    PRIMARY KEY (run_id, key) \
                 )",
            )
            .unwrap();
            conn.execute_batch("ALTER TABLE runs ADD COLUMN brokkr_args TEXT").unwrap();
            // The test fixture predates the variant→mode rename: use the
            // old schema name, the v13→v14 migration will rename it.
            conn.pragma_update(None, "user_version", 13).unwrap();

            // One row per preset family + one unchanged row to check
            // the migration doesn't touch non-preset commands.
            let preset_rows: &[(&str, &str)] = &[
                ("cat-way", "cat"),
                ("cat-relation", "cat"),
                ("cat-dedupe", "cat"),
                ("cat-clean", "cat"),
                ("extract-simple", "extract"),
                ("extract-complete", "extract"),
                ("extract-smart", "extract"),
                ("tags-filter-way", "tags-filter"),
                ("tags-filter-amenity", "tags-filter"),
                ("tags-filter-twopass", "tags-filter"),
                ("tags-filter-osc", "tags-filter"),
                ("getid-refs", "getid"),
                ("getid-invert", "getid"),
                ("inspect-nodes", "inspect"),
                ("inspect-tags", "inspect"),
                ("inspect-tags-way", "inspect"),
                ("diff-osc", "diff"),
                ("sort", "sort"),      // untouched
                ("getid", "getid"),    // already consolidated
                ("inspect", "inspect"),// already consolidated
            ];
            for (old, _) in preset_rows {
                conn.execute(V3_INSERT, rusqlite::params![old, "bench", "pbfhogg"])
                    .unwrap();
            }
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v14 to v15");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let commands: Vec<String> = db
            .conn
            .prepare("SELECT command FROM runs ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        let expected: Vec<&str> = vec![
            "cat", "cat", "cat", "cat", "extract", "extract", "extract", "tags-filter",
            "tags-filter", "tags-filter", "tags-filter", "getid", "getid", "inspect", "inspect",
            "inspect", "diff", "sort", "getid", "inspect",
        ];
        assert_eq!(commands, expected);

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Fresh database gets correct schema version
    // -----------------------------------------------------------------------

    #[test]
    fn fresh_db_has_correct_schema_version() {
        let (dir, db_path) = test_db("fresh_version");

        let db = ResultsDb::open(&db_path).expect("open fresh db");

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        drop(db);
        cleanup(&dir, &db_path);
    }
}
