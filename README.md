# cargo-bench-compare

Benchmark two git revisions of a Rust workspace against each other using git
worktrees. Each revision gets its own detached worktree and target directory, with
`RUSTFLAGS="-C target-cpu=native"` and `--profile release-tuned` by default.

## Installation

Install directly from GitHub with Cargo:

```bash
cargo install --git https://github.com/<owner>/cargo-bench-compare
```

Cargo installs the binary as `cargo-bench-compare`. Cargo discovers subcommands by
looking for `cargo-*` binaries on `PATH`, so make sure Cargo's bin directory is
available system-wide:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

After that, the command is available as:

```bash
cargo bench-compare --help
```

For local development from a checkout, use:

```bash
cargo install --path .
```

## Usage

```bash
# Criterion: compare HEAD (base, implicit) vs one revision (candidate)
cargo bench-compare -p rmc-core --bench hot_path --rev 04afe73

# Criterion: compare two explicit revisions
cargo bench-compare -p rmc-core --bench hot_path --rev 04afe73 --rev-base main

# Binary mode, wall-clock (default metric), 3 reps, pinned to core 2
cargo bench-compare -p rmc-minimal --bin rmc-minimal --args "full 100000000" \
    --reps 3 --rev 04afe73 --runs-on-core 2

# Binary mode with a throughput regex and trailing args after --
cargo bench-compare -p rmc-minimal --bin rmc-minimal --reps 3 --rev 04afe73 \
    --metric-regex 'steps/sec:\s*([\d.]+)' --metric-dir higher -- full 100000000

# Machine-readable
cargo bench-compare -p rmc-core --bench hot_path --rev 04afe73 --json > cmp.json
```

## Future Work

Welch's t-test, `BENCH_RESULT` line protocol, `--cache-dir` warm target dirs
shared by sha, `--fail-on-regression` exit code, lockfile-difference note, and
colors are intentionally out of scope for now.
