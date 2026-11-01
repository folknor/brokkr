#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::PipelineOpts;
use crate::config::project_config::elivagar::TilegenConfig;

fn block(toml_text: &str) -> TilegenConfig {
    toml::from_str(toml_text).unwrap()
}

/// The qualifier is the argv's block flags with each ocean entry under its
/// brokkr.toml name, so a host's data dir never splits it and an edited
/// block always does.
#[test]
fn the_qualifier_is_the_block_argv_with_ocean_names_unresolved() {
    let data = crate::test_scratch::scratch("elivagar-pipeline", "qualifier");
    std::fs::write(data.join("water.shp"), b"").unwrap();
    let opts = PipelineOpts {
        locations_on_ways: true,
        force_sorted: false,
    };
    let tg = block("ocean = [\"z0-z14:water.shp\"]\ncompression_level = 6\nway_budget = \"6G\"\n");
    let mut argv = Vec::new();
    opts.push_args(&tg, &mut argv, &data).unwrap();
    let resolved = format!("z0-z14:{}", data.join("water.shp").display());
    assert_eq!(
        argv.first().map(String::as_str),
        Some("--compression-level")
    );
    assert!(argv.contains(&resolved), "{argv:?}");
    let expected: Vec<String> = argv
        .iter()
        .map(|t| {
            if *t == resolved {
                "z0-z14:water.shp".into()
            } else {
                t.clone()
            }
        })
        .collect();
    assert_eq!(
        opts.arm_qualifier(&tg).unwrap(),
        format!("tilegen={}", serde_json::json!(expected))
    );

    let edited =
        block("ocean = [\"z0-z14:water.shp\"]\ncompression_level = 6\nway_budget = \"2G\"\n");
    assert_ne!(
        opts.arm_qualifier(&tg).unwrap(),
        opts.arm_qualifier(&edited).unwrap()
    );
    // An empty block is still qualified, never the legacy unqualified key.
    let bare = PipelineOpts {
        locations_on_ways: false,
        force_sorted: false,
    };
    assert_eq!(bare.arm_qualifier(&block("")).unwrap(), "tilegen=[]");
}
