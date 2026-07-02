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
# Zero flags: your working tree exactly as it is (uncommitted changes included)
# vs the commit your branch forked from — the everyday "did my work pay off?" call
cargo bench-compare -p rmc-core --bench hot_path

# What did my uncommitted edits change, relative to my last commit?
cargo bench-compare -p rmc-core --bench hot_path --rev :worktree --rev-base HEAD

# Compare one revision (candidate) against the fork point
cargo bench-compare -p rmc-core --bench hot_path --rev 04afe73

# Compare two explicit revisions
cargo bench-compare -p rmc-core --bench hot_path --rev 04afe73 --rev-base v0.3.0

# Evaluate a revision against the current checkout
cargo bench-compare -p rmc-core --bench hot_path --rev pr-branch --rev-base HEAD

# Binary mode, wall-clock (default metric), 3 reps, pinned to core 2
cargo bench-compare -p rmc-minimal --bin rmc-minimal --args "full 100000000" \
    --reps 3 --runs-on-core 2

# Binary mode with a throughput regex and trailing args after --
cargo bench-compare -p rmc-minimal --bin rmc-minimal --reps 3 \
    --metric-regex 'steps/sec:\s*([\d.]+)' --metric-dir higher -- full 100000000

# Machine-readable
cargo bench-compare -p rmc-core --bench hot_path --json > cmp.json
```

## Progress

Binary mode shows a live progress bar on stderr during measurement runs. The bar
spans all `2 × reps` runs — each run fills an equal segment — annotated with the
run counter, side, elapsed time, and ETA. It is automatically disabled when
stderr is not a terminal, and can be disabled with `--no-progress`.

`--progress-regex` adds within-run progress by scraping the binary's stdout or
stderr: the current segment then fills continuously instead of jumping at run
boundaries. Two capture groups are read as `done/total`; one capture group is
read as a percentage from 0 to 100. Named groups `done`/`total`/`percent` also
work. Matching is best-effort; lines that fail to parse are ignored.

```bash
cargo bench-compare -p rmc-minimal --bin rmc-minimal --reps 3 \
    --progress-regex 'step (\d+)/(\d+)' -- full 100000000
```

The benchmarked process pays for its own printing, so rate-limit progress output
to roughly 10 lines/sec to keep the measurement clean.

Builds are summarized the same way: instead of streaming every `Compiling …`
line, a single status line per side shows the crate cargo is currently
compiling, a crate counter, and elapsed time. Cargo's full output still streams
when stderr is not a terminal or with `--no-progress`, and build failures always
print the captured diagnostics.

## Revisions and defaults

`--rev` (candidate) and `--rev-base` (base) accept any commit, branch, or tag,
plus two sentinels. The leading `:` cannot occur in a git ref name, so they can
never collide with one of your branches:

- **`:worktree`** — a snapshot of the working tree exactly as it is, including
  staged, unstaged, and untracked (non-gitignored) changes. The snapshot is a
  temporary dangling commit: your index, stash, history, and files are untouched,
  and the measurement stays consistent even if you keep editing while the
  benchmark runs. On a clean tree this is simply `HEAD`.
- **`:merge-base`** — `git merge-base HEAD <default-branch>`, i.e. the commit the
  current branch forked from. The default branch is resolved as `origin/HEAD` →
  local `main` → local `master`. Using the fork point rather than the branch tip
  keeps commits that landed on the default branch *after* you branched out of
  the comparison.

The defaults are `--rev :worktree --rev-base :merge-base`: the zero-flag call
benchmarks your current work — committed or not — against the point where your
branch left the default branch.

A pleasant corollary on the default branch itself: the merge base of HEAD with
its own branch is HEAD, so the zero-flag call adapts on its own. With uncommitted
changes it measures exactly "my edits vs my last commit"; with a clean tree there
is nothing to measure and it stops with a "nothing to compare" error instead of
benchmarking noise (pass `--rev` or `--rev-base` to pick revisions explicitly).

## Warm builds and disk usage

Builds are warm by default. The tool keeps two persistent worktrees under
`~/.cache/cargo-bench-compare/<repo>/`: `warm-base` and `warm-candidate`. Their
`target/` directories stay in place, so repeated runs usually leave the base side
as a no-op and rebuild only crates changed by the candidate.

Use `--cold` when you need guaranteed from-scratch builds in fresh temporary
worktrees. Warm caches can grow to several GB because each side has its own target
directory; reclaim them with:

```bash
cargo bench-compare cache list
cargo bench-compare cache clean
cargo bench-compare cache clean --all
```

The warm worktrees are normal git worktrees and appear in `git worktree list`.
Manually removing them is safe; the next warm run recreates them.

## CPU pinning and frequency governor

On Linux, measurement runs are pinned with `taskset` by default
(`--runs-on-core 0`). Use `--runs-on-core <N>` to choose a different core, or
`--no-pin` to run unpinned. The tool warns when the measured core is not using
the `performance` CPU governor. If a run is unpinned, it checks all visible CPU
cores instead. Systems without cpufreq support, common in some VMs and
containers, stay silent because there is no governor to change.

Pass `--set-governor` to temporarily set the pinned core's governor to
`performance` for the run. This is opt-in because it may prompt for sudo. The
previous governor is restored automatically on exit, including Ctrl-C. A process
killed with SIGKILL cannot restore it; fix that manually with:

```bash
echo <previous-governor> | sudo tee /sys/devices/system/cpu/cpu<N>/cpufreq/scaling_governor
```

## Known Limitations

- Profile detection is a simple text match in the workspace `Cargo.toml`: a
  commented `[profile.release-tuned]` header can suppress automatic profile
  injection, while profiles defined only in `.cargo/config.toml` may be injected
  redundantly.
- `:worktree` snapshots do not capture dirty *submodule* contents; the snapshot
  records the submodule commit currently in the index.

## Future Work

Welch's t-test, `BENCH_RESULT`/`BENCH_PROGRESS` line protocol (opt-in via a
`BCMP_PROGRESS` env var), `--fail-on-regression` exit code, lockfile-difference
note, and colors are intentionally out of scope for now.
