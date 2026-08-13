# `brokkr bench`

The criterion runner. Not project-gated: any Rust repo with criterion bench
targets, like `run`, `deps` and `clippy`.

```
brokkr bench                          # list the workspace's bench targets
brokkr bench NAME                     # measure, saving a baseline named for HEAD
brokkr bench NAME --commit REF        # measure REF in a worktree
brokkr bench NAME --compare A B       # diff two stored baselines, sampling nothing
brokkr bench --baselines              # list what has been recorded
```

## Measurement and comparison are separate verbs

`cargo bench` with no baseline flags writes to a baseline literally called
`base` and compares against whatever `base` held before. Every run therefore
destroys the previous reference. That is fine for a tight edit loop and useless
for studying a series of commits: the second measurement eats the first.

So `brokkr bench` splits the two operations. A measuring run always saves under
a name, and comparison is a separate invocation that samples nothing -
criterion's `--load-baseline` supplies the new side from storage instead of
measuring it. The expensive work happens once per commit, and you can ask as
many questions of the result as you like afterwards.

A three-commit study is therefore three measurements and as many comparisons as
you want:

```
brokkr bench black_scholes --commit 03062cce63
brokkr bench black_scholes --commit 8fa769d4eb
brokkr bench black_scholes
brokkr bench black_scholes --compare 03062cce 8fa769d4
```

`--compare A B` reads *B against A*: `A` is the reference, `B` is the side being
judged. Same verb and same argument order as `brokkr results --compare`.

Comparison still builds and launches the bench binary - "no sampling" means
cheap, not instant.

## Baseline names

A clean tree names its baseline after the short commit hash: stable,
meaningful, and recoverable from the git log.

A dirty tree is **refused** unless you pass `--name LABEL`. The obvious
fallback of `<hash>-dirty` is the worst option available, because
edit-measure-edit-measure is the most common way to use this command and every
iteration would silently overwrite the last - destroying data in exactly the
workflow that generates the most of it. `--name` is also how you label a
baseline for a reason git cannot express.

## The environment gate

A criterion baseline is a set of timings with no record of how the code that
produced them was built. Compare one measured under `-Ctarget-cpu=native` on a
toolchain from March against one from a different toolchain or a different
machine, and criterion reports a confident percentage that means nothing: it
has no way to know the two are not comparable, so it does not say. At the effect
sizes this command exists to resolve - a percent or two, near criterion's own
1% default noise threshold - a toolchain bump is larger than the signal, and
nothing in the output would look wrong.

Each saved baseline therefore gets a stamp recording `rustc` version, host
triple, CPU model, any `RUSTFLAGS` in the environment, and a digest of
`~/.cargo/config.toml`. `--compare` refuses when the two disagree. `--lenient`
downgrades the refusal to a warning, because "these differ and I know why" is a
legitimate position - what is not legitimate is not being told.

Two limits worth knowing:

- Flags set in `~/.cargo/config.toml` are not exposed by any stable cargo
  interface, so the stamp records a **digest of the file** rather than the flags
  themselves. It can say the file changed, not which flag changed. That is
  enough to refuse a comparison, which is the job.
- Only fields **both** stamps recorded are compared. A field one side never had
  is not a difference, so adding a new field later does not retroactively
  invalidate every existing baseline.

## Where baselines live

Under `.brokkr/bench/`, via criterion's `CRITERION_HOME`, rather than the
default `target/criterion`.

Baselines are results, not build artifacts. They cost minutes each and are not
reconstructible from the source tree, so they must not sit in a directory whose
entire purpose is being safe to delete. Keeping them in a brokkr-designated
directory means `clean` spares them under the existing rule rather than by a
special case, and a user's own `cargo clean` cannot reach them at all.

`--baselines` lists brokkr's own stamp store, not criterion's directory tree -
so this command only ever reads files it wrote, and a change to criterion's
on-disk layout cannot break the listing.

## The invocation brokkr constructs

```
cargo bench -p <owning package> --bench <target> --no-fail-fast -- <criterion args>
```

`-p` is derived from the bench target's own package, never supplied by the
caller. That is not a convenience: building several bench targets in one cargo
invocation can link crates declaring `panic = "abort"` against a harness
compiled to unwind, which fails at link time. Upstream projects usually document
a one-crate-at-a-time rule for this; deriving the package makes it structural,
so there is no way to phrase an invocation that violates it.

Everything after `--` is forwarded to the criterion harness untouched. That is
where `--sample-size`, `--measurement-time` and `--noise-threshold` go - exactly
the knobs a near-noise-floor effect needs, which is why they are not fixed in
brokkr.

## Toolchain

`bench` takes the global lock, so a `disable_toolchain` project's pinned
`rust-toolchain.toml` is moved aside for the window cargo runs in - the same
mechanism every build path uses. A `--commit` run re-arms the disable at the
worktree before taking the lock, so the pin moved aside is the one in the tree
being built rather than the live root's.

## Criterion versus iai

Nothing distinguishes a criterion bench target from an iai (or plain libtest)
one at discovery time. Both are `kind: ["bench"]` in cargo metadata and both set
`harness = false` in the manifest, so neither source separates them. The only
reliable discriminator is behavioural - criterion implements `--list`, iai does
not - and asking costs a build.

So the index lists **every** bench target, and a baseline verb aimed at a
non-criterion one fails when the child rejects the flag. In a workspace mixing
the two, the target names are the only guide.
