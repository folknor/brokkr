#![allow(clippy::unwrap_used, clippy::panic)]

use super::*;

fn composed(text: &str) -> toml::Table {
    toml::from_str(text).unwrap()
}

fn at(path: &[&str]) -> Segments {
    path.iter().map(|s| (*s).to_owned()).collect()
}

fn get<'a>(table: &'a toml::Table, path: &[&str]) -> &'a toml::Value {
    let (last, parents) = path.split_last().unwrap();
    let mut current = table;
    for segment in parents {
        current = current[*segment].as_table().unwrap();
    }
    &current[*last]
}

const CONFIG: &str = r#"
project = "p"

[data.workloads.w.bench]
file = "examples/w.lua"
xxhash = "00000000000000000000000000000001"

[box]
data = "/srv/data"
features = ["f"]

[box.datasets.denmark]
download_date = "2026-02-20"

[box.datasets.denmark.pbf.indexed]
file = "dk.osm.pbf"
xxhash = "00000000000000000000000000000002"
seq = 4704

[box.datasets.denmark.osc.4705]
file = "dk-4705.osc.gz"

[box.datasets.denmark.snapshot.s1.pbf.raw]
file = "s1/dk.osm.pbf"

[box.datasets.delivery]
path = "deliveries/a"
xxh128 = "00000000000000000000000000000003"

[other.datasets.elsewhere.pbf.raw]
file = "x.pbf"
"#;

#[test]
fn the_selected_hosts_datasets_resolve_and_other_hosts_drop_out() {
    let view = build(&composed(CONFIG), "box", Path::new("/proj"));
    let table = view.table();
    assert!(!table.contains_key("box") && !table.contains_key("other"));
    assert_eq!(table["project"].as_str(), Some("p"));
    assert_eq!(
        get(table, &["datasets", "denmark", "pbf", "indexed", "file"]).as_str(),
        Some("/srv/data/dk.osm.pbf")
    );
    assert_eq!(
        get(table, &["datasets", "denmark", "pbf", "indexed", "seq"]).as_integer(),
        Some(4704)
    );
    assert_eq!(
        get(
            table,
            &[
                "datasets", "denmark", "snapshot", "s1", "pbf", "raw", "file"
            ]
        )
        .as_str(),
        Some("/srv/data/s1/dk.osm.pbf")
    );
    assert_eq!(
        get(table, &["datasets", "delivery", "path"]).as_str(),
        Some("/proj/deliveries/a")
    );
    assert_eq!(
        get(table, &["data", "workloads", "w", "bench", "file"]).as_str(),
        Some("/proj/examples/w.lua")
    );
    assert_eq!(get(table, &["host", "name"]).as_str(), Some("box"));
    assert_eq!(get(table, &["host", "data"]).as_str(), Some("/srv/data"));
    assert_eq!(
        get(table, &["host", "scratch"]).as_str(),
        Some("/proj/data/scratch")
    );
    assert!(
        !get(table, &["host"])
            .as_table()
            .unwrap()
            .contains_key("datasets")
    );

    let pbf = view
        .file(&at(&["datasets", "denmark", "pbf", "indexed"]))
        .unwrap();
    assert_eq!(pbf.field, "file");
    assert!(!pbf.tree);
    assert_eq!(pbf.pin.as_deref(), Some("00000000000000000000000000000002"));
    assert_eq!(
        pbf.pin_at,
        at(&["box", "datasets", "denmark", "pbf", "indexed", "xxhash"])
    );
    let delivery = view.file(&at(&["datasets", "delivery"])).unwrap();
    assert_eq!((delivery.field, delivery.tree), ("path", true));
    assert_eq!(
        delivery.pin_at,
        at(&["box", "datasets", "delivery", "xxh128"])
    );
    let workload = view
        .file(&at(&["data", "workloads", "w", "bench"]))
        .unwrap();
    assert_eq!(
        workload.pin_at,
        at(&["data", "workloads", "w", "bench", "xxhash"])
    );
    assert!(view.file(&at(&["datasets", "denmark", "pbf"])).is_none());
}

#[test]
fn the_primary_is_snapshot_base_with_the_primarys_pins() {
    let view = build(&composed(CONFIG), "box", Path::new("/proj"));
    let table = view.table();
    let base = get(table, &["datasets", "denmark", "snapshot", "base"])
        .as_table()
        .unwrap();
    assert_eq!(
        get(base, &["pbf", "indexed", "file"]).as_str(),
        Some("/srv/data/dk.osm.pbf")
    );
    assert_eq!(
        get(base, &["osc", "4705", "file"]).as_str(),
        Some("/srv/data/dk-4705.osc.gz")
    );
    assert_eq!(base["download_date"].as_str(), Some("2026-02-20"));
    assert!(
        !base.contains_key("seq"),
        "no snapshot-wide seq is invented"
    );
    let node = view
        .file(&at(&[
            "datasets", "denmark", "snapshot", "base", "pbf", "indexed",
        ]))
        .unwrap();
    assert_eq!(
        node.pin_at,
        at(&["box", "datasets", "denmark", "pbf", "indexed", "xxhash"])
    );
    // A dataset without a primary still has an empty base to select under.
    let delivery_base = get(table, &["datasets", "delivery", "snapshot", "base"])
        .as_table()
        .unwrap();
    assert!(delivery_base["pbf"].as_table().unwrap().is_empty());
    assert!(delivery_base["osc"].as_table().unwrap().is_empty());
}

#[test]
fn the_host_namespace_and_an_unknown_host_resolve_too() {
    let config = "project = \"p\"\n[host.data.datasets.d.pbf.raw]\nfile = \"r.pbf\"\n";
    let view = build(&composed(config), "data", Path::new("/proj"));
    assert_eq!(
        get(view.table(), &["datasets", "d", "pbf", "raw", "file"]).as_str(),
        Some("/proj/data/r.pbf")
    );
    assert_eq!(
        view.file(&at(&["datasets", "d", "pbf", "raw"]))
            .unwrap()
            .pin_at,
        at(&["host", "data", "datasets", "d", "pbf", "raw", "xxhash"])
    );
    let view = build(&composed(CONFIG), "nowhere", Path::new("/proj"));
    assert!(view.table()["datasets"].as_table().unwrap().is_empty());
    assert_eq!(
        get(view.table(), &["host", "data"]).as_str(),
        Some("/proj/data")
    );
}

#[test]
fn dotted_keys_stay_distinct_nodes_and_values_convert() {
    let config = "[data.cases.\"a.b\"]\nfile = \"first\"\n[data.cases.a.b]\nfile = \"second\"\n";
    let view = build(&composed(config), "box", Path::new("/proj"));
    assert_eq!(
        view.file(&at(&["data", "cases", "a.b"])).unwrap().path,
        Path::new("/proj/first")
    );
    assert_eq!(
        view.file(&at(&["data", "cases", "a", "b"])).unwrap().path,
        Path::new("/proj/second")
    );
    assert_eq!(shown(&at(&["data", "cases", "a.b"])), "data.cases.\"a.b\"");
    let odd = shown(&at(&["data", "a\u{0}b\u{1}"]));
    let parsed: toml::Table = toml::from_str(&format!("{odd} = 1")).unwrap();
    assert_eq!(get(&parsed, &["data", "a\u{0}b\u{1}"]).as_integer(), Some(1), "{odd}");

    let big = toml::Value::Integer(i64::MAX);
    assert_eq!(value(&big), lua::Value::Text(i64::MAX.to_string()));
    assert_eq!(value(&toml::Value::Integer(7)), lua::Value::Number(7.0));
}
