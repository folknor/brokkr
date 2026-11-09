#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

/// A project with `body` after its project line, its bench `b`, its sources
/// and the view on host `box`.
struct Fixture {
    dir: PathBuf,
    bench: Bench,
    sources: ConfigSources,
    view: View,
}

fn fixture(test_name: &str, body: &str) -> Fixture {
    let dir = crate::test_scratch::scratch("bench_params", test_name);
    std::fs::write(dir.join("brokkr.toml"), format!("project = \"p\"\n{body}")).unwrap();
    let (_, config, _, sources) = crate::config::load_with_sources(&dir).unwrap();
    let view = view::build(sources.composed(), "box", &dir);
    Fixture {
        bench: config.benches["b"].clone(),
        dir,
        sources,
        view,
    }
}

fn tokens(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// The one arm `words` bind to.
fn one(f: &Fixture, words: &[&str]) -> Bound {
    let given = parse(&f.bench, &tokens(words)).unwrap();
    let mut arms = bind(&f.bench, &given, &f.view).unwrap();
    assert_eq!(arms.len(), 1, "{words:?} fans out");
    arms.remove(0)
}

/// Every arm `words` bind to, each as its params' keys.
fn arms(f: &Fixture, words: &[&str]) -> Vec<BTreeMap<String, Vec<String>>> {
    let given = parse(&f.bench, &tokens(words)).unwrap();
    bind(&f.bench, &given, &f.view)
        .unwrap()
        .iter()
        .map(|arm| {
            arm.document()
                .into_iter()
                .map(|(name, p)| (name, p.values.into_iter().map(|v| v.key).collect()))
                .collect()
        })
        .collect()
}

/// Why `words` are refused.
fn refusal(f: &Fixture, words: &[&str]) -> String {
    let given = parse(&f.bench, &tokens(words)).unwrap();
    bind(&f.bench, &given, &f.view).unwrap_err().to_string()
}

const DATASETS: &str = r#"
[bench.b]
script = "function bench() end"
params.dataset = { from = "datasets", default = "denmark" }
params.snapshot = { from = "datasets.{dataset}.snapshot", default = "base" }
params.variant = { from = "datasets.{dataset}.snapshot.{snapshot}.pbf", default = "indexed" }
params.osc = { from = "datasets.{dataset}.osc", many = true }
params.jobs = { free = true, help = "threads" }

[box.datasets.denmark.pbf.indexed]
file = "dk.pbf"
seq = 4704

[box.datasets.denmark.pbf.raw]
file = "dk-raw.pbf"

[box.datasets.denmark.osc.4705]
file = "dk.osc"

[box.datasets.denmark.snapshot.s1.pbf.raw]
file = "s1.pbf"

[box.datasets.sweden.pbf.raw]
file = "se.pbf"
"#;

#[test]
fn tokens_are_name_value_pairs_and_help_only_where_a_name_stands() {
    let f = fixture("parse", DATASETS);
    let given = parse(&f.bench, &tokens(&["--jobs", "--help", "--variant=raw"])).unwrap();
    assert!(!given.help, "--help taken as --jobs's value is data");
    assert_eq!(given.values["jobs"], ["--help"]);
    assert_eq!(given.values["variant"], ["raw"]);
    assert!(
        parse(&f.bench, &tokens(&["--jobs", "4", "-h"]))
            .unwrap()
            .help
    );

    let err = |words: &[&str]| parse(&f.bench, &tokens(words)).unwrap_err().to_string();
    assert!(
        err(&["--jbos", "4"]).contains("no param --jbos"),
        "{}",
        err(&["--jbos", "4"])
    );
    assert!(err(&["--jobs"]).contains("needs a value"));
    // A blank value is refused when it binds, beside the other refusals.
    let blank = refusal(&f, &["--jobs", " ", "--dataset", "norway"]);
    assert!(
        blank.contains("is blank") && blank.contains("norway"),
        "{blank}"
    );
    let repeated = parse(&f.bench, &tokens(&["--jobs", "1", "--jobs=2,3"])).unwrap();
    assert_eq!(
        repeated.values["jobs"],
        ["1", "2,3"],
        "occurrences in order"
    );
    assert!(err(&["4"]).contains("not a param"));
    assert!(err(&["--"]).contains("not a param"));
}

#[test]
fn params_bind_in_dependency_order_into_the_document() {
    let f = fixture("bind", DATASETS);
    let mut bound = one(&f, &["--osc", "4705"]);
    bound.digest(&f.dir, &f.sources).unwrap_err();
    std::fs::create_dir_all(f.dir.join("data")).unwrap();
    std::fs::write(f.dir.join("data/dk.pbf"), b"pbf").unwrap();
    std::fs::write(f.dir.join("data/dk.osc"), b"osc").unwrap();
    bound.digest(&f.dir, &f.sources).unwrap();
    let doc = bound.document();
    assert_eq!(
        doc.keys().collect::<Vec<_>>(),
        ["dataset", "osc", "snapshot", "variant"],
        "jobs has no value and no default: unbound, absent"
    );
    assert_eq!(doc["dataset"].from.as_deref(), Some("datasets"));
    assert_eq!(doc["dataset"].values[0].key, "denmark");
    assert_eq!(
        doc["dataset"].values[0].digest, None,
        "a container pairs on its key"
    );
    assert_eq!(
        doc["variant"].from.as_deref(),
        Some("datasets.denmark.snapshot.base.pbf")
    );
    let digest = doc["variant"].values[0].digest.clone().unwrap();
    assert_eq!(
        digest,
        crate::preflight::compute_file_digest(&f.dir.join("data/dk.pbf"))
            .unwrap()
            .to_string()
    );
    assert!(doc["osc"].many && doc["osc"].values.len() == 1);
    assert_eq!(
        bound.inputs(),
        [f.dir.join("data/dk.osc"), f.dir.join("data/dk.pbf")]
    );

    let args = bound.args();
    let names: Vec<&str> = args.params.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["dataset", "osc", "snapshot", "variant"]);
    let (_, variant) = &args.params[3];
    let lua::Value::Table(fields) = variant else {
        panic!("{variant:?}")
    };
    let file = fields.iter().find(|(k, _)| k == "file").unwrap();
    assert!(
        matches!(&file.1, lua::Value::Handle(h) if h.index == 1),
        "handles numbered as inputs lists them: {fields:?}"
    );
    let (_, osc) = &args.params[1];
    assert!(matches!(osc, lua::Value::List(items) if items.len() == 1));
    assert_eq!(args.keys[3].1, lua::Value::Text("indexed".into()));
}

#[test]
fn the_dynamic_refusals_name_the_valid_keys() {
    let f = fixture("refusals", DATASETS);
    let refused = |words: &[&str]| refusal(&f, words);
    let err = refused(&["--variant", "locations"]);
    assert!(
        err.contains("not a key under datasets.denmark.snapshot.base.pbf")
            && err.contains("indexed, raw"),
        "{err}"
    );
    // A default that is not under its table, judged only when used.
    let err = refused(&["--dataset", "sweden"]);
    assert!(
        err.contains("default \"indexed\"") && err.contains("raw"),
        "{err}"
    );
    one(&f, &["--dataset", "sweden", "--variant", "raw"]);
    let err = refused(&["--dataset", "norway"]);
    assert!(err.contains("denmark, sweden"), "{err}");
    assert!(
        !err.contains("--variant") && !err.contains("--snapshot"),
        "dependents of a refused parent are not judged: {err}"
    );
    // A plain param's `all` stays the string, and its `..` too.
    let bound = one(&f, &["--jobs", "all"]);
    assert_eq!(bound.document()["jobs"].values[0].key, "all");
    let bound = one(&f, &["--jobs", "1..4"]);
    assert_eq!(bound.document()["jobs"].values[0].key, "1..4");
}

const REQUIRED: &str = r#"
[bench.b]
script = "function bench() end"
params.scale = { from = "data.workloads", default = "bench" }
params.workload = { from = "data.workloads.{scale}", required = true }
params.field = { from = "data.workloads.{scale}.{workload}", default = "nope" }
params.mode = { free = true, required = true }
params.tags = { from = "data.tags", many = true, required = true }

[data.workloads.bench.a]
file = "a.lua"
[data.workloads.bench.b]
file = "b.lua"
[data.workloads.hotpath.a]
file = "ha.lua"
[data.workloads.empty]

[data.tags]
"#;

#[test]
fn a_required_param_absent_is_one_refusal_whatever_its_arms() {
    let f = fixture("required", REQUIRED);
    let bind_err = |words: &[&str]| {
        let given = parse(&f.bench, &tokens(words)).unwrap();
        bind(&f.bench, &given, &f.view).unwrap_err()
    };

    // Every absent required param named, each once; the dependent `field`,
    // whose default is no key, is not judged under the absent `workload`.
    let DevError::Preflight(lines) = bind_err(&[]) else {
        panic!("several absences are a preflight list");
    };
    // In dependency order: `field` pulls `scale` and `workload` first.
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        lines[0],
        "bench b: --workload is required: give it on the command line\n  valid under \
         data.workloads.bench: a, b"
    );
    assert_eq!(
        lines[1], "bench b: --mode is required: give it on the command line",
        "a plain param has no keys to list"
    );
    assert_eq!(
        lines[2],
        "bench b: --tags is required: give it on the command line\n  no keys under data.tags \
         on this host"
    );

    // One absence under several paths is still one refusal, a usage error,
    // its paths detail in arm order; an empty one says so.
    let err = bind_err(&[
        "--mode",
        "m",
        "--tags",
        "all",
        "--scale",
        "hotpath,bench,empty",
    ]);
    let DevError::Usage(message) = &err else {
        panic!("one absence is a usage error: {err:?}");
    };
    assert_eq!(
        message,
        "bench b: --workload is required: give it on the command line\n  valid under \
         data.workloads.hotpath: a\n  valid under data.workloads.bench: a, b\n  no keys \
         under data.workloads.empty on this host"
    );

    // A refused parent adds no detail and does not hide the absence.
    let DevError::Preflight(lines) = bind_err(&["--mode", "m", "--tags", "all", "--scale", "x"])
    else {
        panic!("a refused parent and an absence are two refusals");
    };
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("--scale x selects \"x\""), "{lines:?}");
    assert_eq!(
        lines[1],
        "bench b: --workload is required: give it on the command line"
    );

    // Given blank is the blank refusal, not an absence.
    let err = refusal(
        &f,
        &[
            "--mode",
            "m",
            "--tags",
            "all",
            "--workload",
            " ",
            "--field",
            "file",
        ],
    );
    assert!(
        err.contains("is blank") && !err.contains("required"),
        "{err}"
    );

    // Given, it binds; a required collection may bind empty.
    let bound = one(
        &f,
        &[
            "--mode",
            "m",
            "--tags",
            "all",
            "--workload",
            "a",
            "--field",
            "file",
        ],
    );
    let doc = bound.document();
    assert_eq!(doc["workload"].values[0].key, "a");
    assert!(
        doc["tags"].values.is_empty(),
        "an empty collection is given"
    );
}

#[test]
fn help_names_a_required_absence_once() {
    let f = fixture("required_help", REQUIRED);
    // Help never fails over it: a dependent's keys are unknown, the absence
    // said once.
    let help = parse(&f.bench, &tokens(&["--help"])).unwrap();
    let Some(Err(why)) = values_for(&f.bench, &help, &f.view, "field") else {
        panic!("field's keys need --workload");
    };
    assert_eq!(why.matches("--workload is required").count(), 1, "{why}");
    let Some(Ok(paths)) = values_for(&f.bench, &help, &f.view, "workload") else {
        panic!("workload's own keys are listed");
    };
    assert_eq!(paths[0].1, ["a", "b"]);
    // Said once under several arms too.
    let help = parse(
        &f.bench,
        &tokens(&["--scale", "hotpath,bench,empty", "--help"]),
    )
    .unwrap();
    let Some(Err(why)) = values_for(&f.bench, &help, &f.view, "field") else {
        panic!("field's keys need --workload");
    };
    assert_eq!(why.matches("--workload is required").count(), 1, "{why}");
    assert!(
        why.contains("valid under data.workloads.hotpath: a"),
        "{why}"
    );
}

const COLLECTIONS: &str = r#"
[bench.b]
script = "function bench() end"
params.dataset = { from = "datasets", default = "denmark" }
params.osc = { from = "datasets.{dataset}.osc", many = true }
params.words = { free = true, many = true }

[box.datasets.denmark.pbf.raw]
file = "dk.pbf"

[box.datasets.denmark.osc.10]
file = "10.osc"
[box.datasets.denmark.osc.4705]
file = "a.osc"
[box.datasets.denmark.osc.4706]
file = "b.osc"
[box.datasets.denmark.osc.4707]
file = "c.osc"

[box.datasets.sweden.pbf.raw]
file = "se.pbf"
"#;

#[test]
fn a_collection_binds_lists_ranges_repeats_and_all_in_order() {
    let f = fixture("collections", COLLECTIONS);
    let osc = |words: &[&str]| arms(&f, words).remove(0)["osc"].clone();
    assert_eq!(
        osc(&["--osc", "4707,4705..4706"]),
        ["4707", "4705", "4706"],
        "command-line order, a range ascending in place"
    );
    assert_eq!(osc(&["--osc", "4706", "--osc", "4705"]), ["4706", "4705"]);
    assert_eq!(
        osc(&["--osc", "all"]),
        ["10", "4705", "4706", "4707"],
        "numeric order when every key is a canonical decimal"
    );
    assert_eq!(osc(&["--osc", "4706..4706"]), ["4706"]);
    let bound = one(&f, &["--osc", "4705,4707"]);
    assert_eq!(bound.inputs().len(), 2, "every value is an input");
    let lua::Value::List(entries) = &bound.args().params[1].1 else {
        panic!("a collection is a list")
    };
    assert_eq!(entries.len(), 2);

    let refused = |words: &[&str]| refusal(&f, words);
    let err = refused(&["--osc", "4705,4705..4706"]);
    assert!(err.contains("\"4705\" twice"), "{err}");
    let err = refused(&["--osc", "4705..4710"]);
    assert!(err.contains("lacking 4708, 4709, 4710"), "{err}");
    let err = refused(&["--osc", "0..18446744073709551615"]);
    assert!(err.contains("and 18446744073709551607 more"), "{err}");
    assert!(refused(&["--osc", "04705..4706"]).contains("leading zero"));
    assert!(refused(&["--osc", "+1..2"]).contains("leading zero"));
    assert!(refused(&["--osc", "4706..4705"]).contains("backwards"));
    assert!(refused(&["--osc", "1..18446744073709551616"]).contains("at most"));
    assert!(refused(&["--osc", "all,4705"]).contains("beside"));
    assert!(refused(&["--osc", "all", "--osc", "4705"]).contains("beside"));
    assert!(refused(&["--osc", "4705,"]).contains("blank item"));
    let err = refused(&["--osc", "4705,4799"]);
    assert!(
        err.contains("\"4799\", not a key under datasets.denmark.osc"),
        "{err}"
    );
    // A plain collection takes lists and repeats; it has no keys, so `all`
    // is a string and a range is refused.
    assert_eq!(
        arms(&f, &["--words", "x,all", "--words", "y"]).remove(0)["words"],
        ["x", "all", "y"]
    );
    assert!(refused(&["--words", "1..3"]).contains("plain collection"));
    assert!(refused(&["--words", "x,x"]).contains("twice"));
}

#[test]
fn fanned_params_make_one_arm_per_value_resolved_per_parent() {
    let f = fixture("fanout", DATASETS);
    let arms_of = |words: &[&str]| {
        arms(&f, words)
            .into_iter()
            .map(|arm| {
                ["dataset", "variant", "jobs"]
                    .iter()
                    .filter_map(|p| arm.get(*p).map(|keys| keys.join(",")))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        arms_of(&["--dataset", "all", "--variant", "all"]),
        ["denmark indexed", "denmark raw", "sweden raw"],
        "parents outer, each dataset's own variants"
    );
    assert_eq!(
        arms_of(&["--variant", "raw", "--jobs", "4,1", "--jobs", "2"]),
        ["denmark raw 4", "denmark raw 1", "denmark raw 2"],
        "a plain param fans out on commas and repeats"
    );
    let given = parse(
        &f.bench,
        &tokens(&["--dataset", "denmark,sweden", "--variant", "raw"]),
    )
    .unwrap();
    let bound = bind(&f.bench, &given, &f.view).unwrap();
    assert_eq!(
        arm_labels(&bound),
        ["dataset=denmark", "dataset=sweden"],
        "labels name only what varies"
    );
    assert_eq!(arm_labels(&bound[..1]), [""]);

    // A default missing under one parent refuses the whole invocation.
    let err = refusal(&f, &["--dataset", "denmark,sweden"]);
    assert!(
        err.contains("default \"indexed\"") && err.contains("datasets.sweden"),
        "{err}"
    );
    let err = refusal(&f, &["--dataset", "sweden,denmark", "--variant", "indexed"]);
    assert!(err.contains("datasets.sweden"), "{err}");
    assert!(refusal(&f, &["--jobs", "1,1"]).contains("twice"));
    // Independent refusals are all named.
    let err = refusal(&f, &["--dataset", "norway", "--jobs", "1,,2"]);
    assert!(
        err.contains("preflight failed") && err.contains("norway") && err.contains("blank item"),
        "{err}"
    );
}

#[test]
fn a_fan_out_over_no_keys_is_refused_and_defaults_may_fan_out() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.group = { from = "data.groups" }
params.item = { from = "data.groups.{group}.items" }
params.n = { free = true, default = "1,2" }
params.coll = { from = "data.groups.{group}.items", many = true }

[data.groups.a.items.x]
v = 1
[data.groups.b.items]
"#;
    let f = fixture("emptyfan", body);
    let err = refusal(&f, &["--group", "all", "--item", "all"]);
    assert!(
        err.contains("selects no key") && err.contains("data.groups.b.items"),
        "{err}"
    );
    assert_eq!(
        arms(&f, &["--group", "a", "--item", "all"]).len(),
        2,
        "n=1,2"
    );
    // A collection over no keys is a value: one empty list per arm.
    let bound = arms(&f, &["--group", "b", "--coll", "all"]);
    assert_eq!(bound.len(), 2);
    assert!(bound[0]["coll"].is_empty());
}

#[test]
fn every_entry_is_judged_against_its_own_pin_and_every_refusal_named() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.w = { from = "data.w" }
params.c = { from = "data.w", many = true }

[data.w.x]
file = "f.bin"
[data.w.y]
file = "f.bin"
xxhash = "00000000000000000000000000000000"
[data.w.z]
file = "missing.bin"
[data.w.q]
file = "gone.bin"
"#;
    let f = fixture("sharedpin", body);
    std::fs::write(f.dir.join("f.bin"), b"f").unwrap();
    let given = parse(&f.bench, &tokens(&["--w", "x,y"])).unwrap();
    let mut bound = bind(&f.bench, &given, &f.view).unwrap();
    assert_eq!(union_inputs(&bound), [f.dir.join("f.bin")]);
    let err = digest_arms(&mut bound, Depth::Every, &f.dir, &f.sources)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("--w y") && err.contains("data.w.y.xxhash") && !err.contains("--w x"),
        "{err}"
    );
    let given = parse(&f.bench, &tokens(&["--w", "x"])).unwrap();
    let mut bound = bind(&f.bench, &given, &f.view).unwrap();
    digest_arms(&mut bound, Depth::Every, &f.dir, &f.sources).unwrap();
    // Within one arm too, a refused file does not hide the next.
    let given = parse(&f.bench, &tokens(&["--c", "z,q"])).unwrap();
    let mut bound = bind(&f.bench, &given, &f.view).unwrap();
    let err = digest_arms(&mut bound, Depth::Every, &f.dir, &f.sources)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("missing.bin") && err.contains("gone.bin"),
        "{err}"
    );
}

#[test]
fn a_dotted_key_is_refused_only_where_it_is_substituted() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.group = { free = true }
params.case = { from = "data.{group}" }
params.leaf = { from = "data.cases" }

[data.cases."v1.2"]
count = 10
"#;
    let f = fixture("dotted", body);
    let bound = one(&f, &["--leaf", "v1.2"]);
    assert_eq!(bound.document()["leaf"].values[0].key, "v1.2");
    let given = parse(&f.bench, &tokens(&["--group", "a.b", "--case", "x"])).unwrap();
    let err = bind(&f.bench, &given, &f.view).unwrap_err().to_string();
    assert!(err.contains("holds a '.'"), "{err}");
    // Unused, the dotted value restricts nothing.
    let given = parse(&f.bench, &tokens(&["--group", "a.b"])).unwrap();
    bind(&f.bench, &given, &f.view).unwrap();
    let given = parse(&f.bench, &tokens(&["--case", "x"])).unwrap();
    let err = bind(&f.bench, &given, &f.view).unwrap_err().to_string();
    assert!(err.contains("needs --group"), "{err}");
}

#[test]
fn a_drifted_pin_is_refused_naming_where_it_was_set() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.w = { from = "data.workloads", default = "small" }

[data.workloads.small]
file = "w.lua"
xxhash = "00000000000000000000000000000000"
"#;
    let f = fixture("pin", body);
    std::fs::write(f.dir.join("w.lua"), b"return 1").unwrap();
    let mut bound = one(&f, &[]);
    let err = bound.digest(&f.dir, &f.sources).unwrap_err().to_string();
    assert!(
        err.contains("hash mismatch")
            && err.contains("data.workloads.small.xxhash")
            && err.contains("brokkr.toml"),
        "{err}"
    );
}

/// `a` depends on `b`, which depends on `c`: alphabetical binding would
/// meet each before its parent.
#[test]
fn a_reversed_chain_binds_parents_first() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.a = { from = "data.{b}", default = "x" }
params.b = { from = "data.{c}", default = "m" }
params.c = { from = "data", default = "k" }

[data.k.m]
v = 1
[data.m.x]
v = 2
"#;
    let f = fixture("chain2", body);
    let bound = one(&f, &[]);
    let doc = bound.document();
    assert_eq!(doc["c"].from.as_deref(), Some("data"));
    assert_eq!(doc["b"].from.as_deref(), Some("data.k"));
    assert_eq!(doc["a"].from.as_deref(), Some("data.m"));
}

#[test]
fn nested_entries_bad_pins_and_special_files_are_judged() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.inner = { from = "data.inputs.outer", default = "child" }
params.bad = { from = "data.pins", default = "p" }

[data.inputs.outer]
file = "outer.bin"

[data.inputs.outer.child]
file = "child.bin"

[data.pins.p]
file = "p.bin"
xxhash = 123
"#;
    let f = fixture("nested", body);
    std::fs::write(f.dir.join("child.bin"), b"c").unwrap();
    std::fs::write(f.dir.join("p.bin"), b"p").unwrap();
    let mut bound = one(&f, &[]);
    assert_eq!(
        bound.inputs(),
        [f.dir.join("p.bin"), f.dir.join("child.bin")]
    );
    let err = bound.digest(&f.dir, &f.sources).unwrap_err().to_string();
    assert!(
        err.contains("not a digest") && err.contains("data.pins.p.xxhash"),
        "{err}"
    );

    let body = r#"
[bench.b]
script = "function bench() end"
params.dataset = { from = "datasets", default = "null" }

[box.datasets.null]
path = "/dev/null"
"#;
    let f = fixture("special", body);
    let mut bound = one(&f, &[]);
    let err = bound.digest(&f.dir, &f.sources).unwrap_err().to_string();
    assert!(err.contains("not a regular file or a directory"), "{err}");
}

#[test]
fn help_lists_keys_under_the_parents_given_or_default() {
    let f = fixture("help", DATASETS);
    let keys = |words: &[&str], name: &str| {
        let given = parse(&f.bench, &tokens(words)).unwrap();
        values_for(&f.bench, &given, &f.view, name)
    };
    let listed = |words: &[&str], name: &str| {
        keys(words, name)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(path, keys)| format!("{path}: {}", keys.join(",")))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(&[], "variant"),
        ["datasets.denmark.snapshot.base.pbf: indexed,raw"]
    );
    assert_eq!(
        listed(&["--snapshot", "s1"], "variant"),
        ["datasets.denmark.snapshot.s1.pbf: raw"]
    );
    assert_eq!(
        listed(&[], "snapshot"),
        ["datasets.denmark.snapshot: base,s1"]
    );
    assert!(keys(&[], "jobs").is_none());
    // A fanned parent lists each path it leads to.
    assert_eq!(
        listed(&["--dataset", "all"], "variant"),
        [
            "datasets.denmark.snapshot.base.pbf: indexed,raw",
            "datasets.sweden.snapshot.base.pbf: raw"
        ]
    );
    let err = keys(&["--dataset", "norway"], "variant")
        .unwrap()
        .unwrap_err();
    assert!(err.contains("norway"), "{err}");
}

/// `split = false` takes each occurrence, and the default, whole: a value
/// may hold a `,`, repeats still fan out or fill a collection, and `all` and
/// ranges keep their meaning (brokkr#141).
#[test]
fn an_unsplit_param_takes_each_value_whole() {
    let f = fixture(
        "unsplit",
        r#"
[bench.b]
script = "function bench() end"
params.expr = { free = true, split = false, default = "w/highway=primary,secondary" }
params.exprs = { free = true, many = true, split = false }
params.mode = { choices = ["a,b", "c"], split = false, default = "a,b" }
params.key = { from = "data.keys", split = false, default = "x,y" }
params.nums = { from = "data.nums", many = true, split = false }
params.sub = { from = "data.keys.{key}", split = false }

[data.nums.1]
[data.nums.2]
[data.nums.4]

[data.keys."x,y"]
v = 1
[data.keys.z]
v = 2
"#,
    );
    let arm = arms(&f, &[]).remove(0);
    assert_eq!(arm["expr"], ["w/highway=primary,secondary"]);
    assert_eq!(arm["mode"], ["a,b"]);
    assert_eq!(arm["key"], ["x,y"], "a key holding a comma is reachable");
    assert_eq!(
        arms(&f, &["--exprs", "n/a=1,2", "--exprs", "w/b"]).remove(0)["exprs"],
        ["n/a=1,2", "w/b"],
        "a collection fills from repeats"
    );
    let fanned: Vec<Vec<String>> = arms(&f, &["--expr", "a,b", "--expr", "c"])
        .into_iter()
        .map(|arm| arm["expr"].clone())
        .collect();
    assert_eq!(fanned, [["a,b"], ["c"]], "repeats fan out, commas kept");
    assert_eq!(arms(&f, &["--key", "all"]).len(), 2, "all still expands");
    assert_eq!(
        arms(&f, &["--nums", "1..2", "--nums", "4"]).remove(0)["nums"],
        ["1", "2", "4"],
        "a range still expands"
    );
    assert!(refusal(&f, &["--nums", "1..2", "--nums", "2"]).contains("twice"));
    assert!(refusal(&f, &["--nums", "1,2"]).contains("not a key"));
    assert_eq!(
        arms(&f, &["--sub", "v"]).remove(0)["sub"],
        ["v"],
        "a comma-bearing parent key resolves a dependent path"
    );
    let err = refusal(&f, &["--mode", "c,a,b"]);
    assert!(err.contains("--mode c,a,b"), "{err}");
    let err = refusal(&f, &["--expr", "x", "--expr", " "]);
    assert!(err.contains("--expr x --expr  "), "{err}");
}
