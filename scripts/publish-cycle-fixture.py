#!/usr/bin/env python3
"""Write a throwaway 3-crate workspace shaped like the cycle brokkr's
`publish_cycle` phase exists to catch:

    exec -[dev]-> testkit -> trading -> exec

Usage: publish-cycle-fixture.py <outdir> [--versioned]

Without `--versioned` the dev-dependency is path-only, which cargo strips
on publish - so there is no publication cycle and `brokkr deps` must stay
quiet. With `--versioned` the dev-dep names a version, the edge survives
into the published manifest, and the phase must report the loop.

Writes files only; run `brokkr deps` in <outdir> yourself.
"""

import pathlib
import shutil
import sys

MANIFEST = """[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[{section}]
{dep_name} = {{ path = "../{dep_name}"{version} }}
"""

LEAF = """[package]
name = "{name}"
version = "0.1.0"
edition = "2021"
"""


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    out = pathlib.Path(sys.argv[1])
    versioned = "--versioned" in sys.argv[2:]

    if out.exists():
        shutil.rmtree(out)
    out.mkdir(parents=True)

    (out / "Cargo.toml").write_text(
        '[workspace]\nmembers = ["exec", "testkit", "trading"]\nresolver = "2"\n'
    )
    # brokkr.toml so project detection succeeds; the phase is not gated.
    (out / "brokkr.toml").write_text('project = "pbfhogg"\n')

    crates = [
        ("exec", "testkit", "dev-dependencies", ', version = "0.1.0"' if versioned else ""),
        ("testkit", "trading", "dependencies", ', version = "0.1.0"'),
        ("trading", "exec", "dependencies", ', version = "0.1.0"'),
    ]
    for name, dep_name, section, version in crates:
        crate = out / name
        (crate / "src").mkdir(parents=True)
        (crate / "src" / "lib.rs").write_text("")
        (crate / "Cargo.toml").write_text(
            MANIFEST.format(
                name=name, section=section, dep_name=dep_name, version=version
            )
        )

    print(f"wrote {out} (dev-dep {'versioned' if versioned else 'path-only'})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
