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

const DATASETS: &str = r#"
[bench.b]
script = "function bench() end"
params.dataset = { from = "datasets", default = "denmark" }
params.snapshot = { from = "datasets.{dataset}.snapshot", default = "base" }
params.variant = { from = "datasets.{dataset}.snapshot.{snapshot}.pbf", default = "indexed" }
params.osc = { from = "datasets.{dataset}.osc", many = true }
params.jobs = { help = "threads" }

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
    assert_eq!(given.values["jobs"], "--help");
    assert_eq!(given.values["variant"], "raw");
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
    assert!(err(&["--jobs", " "]).contains("blank"));
    assert!(err(&["--jobs", "1", "--jobs", "2"]).contains("twice"));
    assert!(err(&["4"]).contains("not a param"));
    assert!(err(&["--"]).contains("not a param"));
}

#[test]
fn params_bind_in_dependency_order_into_the_document() {
    let f = fixture("bind", DATASETS);
    let given = parse(&f.bench, &tokens(&["--osc", "4705"])).unwrap();
    let mut bound = bind(&f.bench, &given, &f.view).unwrap();
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
    let refused = |words: &[&str]| {
        let given = parse(&f.bench, &tokens(words)).unwrap();
        bind(&f.bench, &given, &f.view).unwrap_err().to_string()
    };
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
    let given = parse(
        &f.bench,
        &tokens(&["--dataset", "sweden", "--variant", "raw"]),
    )
    .unwrap();
    bind(&f.bench, &given, &f.view).unwrap();
    let err = refused(&["--dataset", "norway"]);
    assert!(err.contains("denmark, sweden"), "{err}");
    // Lists, ranges and `all` are not read as one key.
    assert!(refused(&["--osc", "4705,4706"]).contains("list"));
    assert!(refused(&["--osc", "4705..4706"]).contains("range"));
    assert!(refused(&["--variant", "all"]).contains("every key"));
    assert!(refused(&["--jobs", "1,2"]).contains("list"));
    let given = parse(&f.bench, &tokens(&["--jobs", "all"])).unwrap();
    bind(&f.bench, &given, &f.view).unwrap();
}

#[test]
fn a_dotted_key_is_refused_only_where_it_is_substituted() {
    let body = r#"
[bench.b]
script = "function bench() end"
params.group = {}
params.case = { from = "data.{group}" }
params.leaf = { from = "data.cases" }

[data.cases."v1.2"]
count = 10
"#;
    let f = fixture("dotted", body);
    let given = parse(&f.bench, &tokens(&["--leaf", "v1.2"])).unwrap();
    let bound = bind(&f.bench, &given, &f.view).unwrap();
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
    let mut bound = bind(&f.bench, &Given::default(), &f.view).unwrap();
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
    let bound = bind(&f.bench, &Given::default(), &f.view).unwrap();
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
    let mut bound = bind(&f.bench, &Given::default(), &f.view).unwrap();
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
    let mut bound = bind(&f.bench, &Given::default(), &f.view).unwrap();
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
    assert_eq!(keys(&[], "variant").unwrap().unwrap(), ["indexed", "raw"]);
    assert_eq!(
        keys(&["--snapshot", "s1"], "variant").unwrap().unwrap(),
        ["raw"]
    );
    assert_eq!(keys(&[], "snapshot").unwrap().unwrap(), ["base", "s1"]);
    assert!(keys(&[], "jobs").is_none());
}
