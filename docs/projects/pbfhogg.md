# pbfhogg

pbfhogg uses script benches declared in `[bench.*]`, run with
`brokkr bench NAME`. See `docs/commands/bench.md` and
`docs/brokkr.toml.bench.md` for params, inputs, profiling and results.

The generic commands remain available: `check`, `test`, `clippy`, `run`,
`build`, `install`, `results`, `sidecar`, `clean` and the other shared tools.
For example:

```
brokkr bench
brokkr bench NAME --help
brokkr run --with pbfhogg pbfhogg-verify -- all denmark
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark
brokkr run --with pbfhogg-harness pbfhogg-library -- --help
```

Answer checking belongs to pbfhogg's `pbfhogg-verify`. Dataset download and
snapshot promotion belong to `pbfhogg-datasets`; library verification belongs
to `pbfhogg-library`. These programs receive the executable built by `--with`
in `BROKKR_WITH_EXE`.

Historical typed-command results and sidecar records remain readable through
`results` and `sidecar`. The dataset `pbf.<variant>.expect`, dataset `expect`,
`getid_ids` and `getparents_ids` fields remain accepted and validated for
compatibility, but brokkr no longer uses them. Script benches still see
configured data in the resolved view.
