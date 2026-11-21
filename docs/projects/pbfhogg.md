# pbfhogg

This page covers how brokkr serves the pbfhogg project. pbfhogg declares
script benches in `[bench.*]` and runs them with `brokkr bench NAME`. Params,
inputs, profiling and results are documented in `brokkr man bench` and
`brokkr man bench-config`.

The generic commands also work here: `check`, `test`, `clippy`, `run`,
`build`, `install`, `results`, `sidecar`, `clean` and the other shared tools.
These invocations list the benches, show one bench's help, and run pbfhogg's
own tools:

```sh
brokkr bench
brokkr bench NAME --help
brokkr run --with pbfhogg pbfhogg-verify -- all denmark
brokkr run --with pbfhogg pbfhogg-datasets -- download denmark
brokkr run --with pbfhogg-harness pbfhogg-library -- --help
```

pbfhogg's own programs own these jobs:

- **`pbfhogg-verify`**: answer checking
- **`pbfhogg-datasets`**: dataset download and snapshot promotion
- **`pbfhogg-library`**: library verification

Each program receives the executable built by `--with` in `BROKKR_WITH_EXE`.

`results` and `sidecar` still read historical typed-command results and
sidecar records. brokkr accepts and validates the dataset fields
`pbf.VARIANT.expect`, `expect`, `getid_ids` and `getparents_ids` for
compatibility, but doesn't use them. Script benches still see configured
data in the resolved view.
