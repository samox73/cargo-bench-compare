# cargo-bench-compare

Benchmark two git revisions of a Rust workspace against each other using git
worktrees. Each revision gets its own detached worktree and target directory, with
`RUSTFLAGS="-C target-cpu=native"` and `--profile release-tuned` by default.

## Installation

Install directly from GitHub with Cargo:

```bash
cargo install --git https://github.com/samox73/cargo-bench-compare
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

## Shell Completions

Install completions with the built-in generator:

```bash
cargo bench-compare completions nushell --install
cargo bench-compare completions bash --install
cargo bench-compare completions zsh --install
cargo bench-compare completions fish --install
```

Nushell completions cover both `cargo bench-compare` and
`cargo-bench-compare`, including dynamic values for revisions, packages,
bench/bin targets, profiles, and `--metric-dir`. Bash, zsh, fish, elvish, and
PowerShell static completions apply to the standalone `cargo-bench-compare`
form; those shells do not get dynamic values in this tier.

To print a script instead of installing it:

```bash
cargo bench-compare completions nushell
cargo bench-compare completions bash
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

## Known Limitations

Profile detection is a simple text match in the workspace `Cargo.toml`: a commented
`[profile.release-tuned]` header can suppress automatic profile injection, while
profiles defined only in `.cargo/config.toml` may be injected redundantly.

## Future Work

Welch's t-test, `BENCH_RESULT` line protocol, `--cache-dir` warm target dirs
shared by sha, `--fail-on-regression` exit code, lockfile-difference note, and
colors are intentionally out of scope for now.
