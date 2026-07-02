#!/usr/bin/env bash
set -euo pipefail
dir="${1:?usage: make_fixture.sh <dir>}"
rm -rf "$dir"; mkdir -p "$dir"; cd "$dir"
git init -q -b main
cargo init -q --name fixture .
git add -A && git commit -qm init

# commit A: fast sleep + metric 100
cat > src/main.rs <<'EOF'
fn main() {
    std::thread::sleep(std::time::Duration::from_millis(100));
    println!("steps/sec: 100");
}
EOF
git add -A && git commit -qm "A: 100ms, metric 100"
git tag fixture-a

# commit B: slow sleep + metric 110
cat > src/main.rs <<'EOF'
fn main() {
    std::thread::sleep(std::time::Duration::from_millis(200));
    println!("steps/sec: 110");
}
EOF
git add -A && git commit -qm "B: 200ms, metric 110"
git tag fixture-b

# commit C: criterion bench, fib(20)
cat >> Cargo.toml <<'EOF'

[dev-dependencies]
criterion = "0.5"

[[bench]]
name = "fixture_bench"
harness = false
EOF
mkdir -p benches
cat > benches/fixture_bench.rs <<'EOF'
use criterion::{criterion_group, criterion_main, Criterion};

fn bench(c: &mut Criterion) {
    c.bench_function("fib_20", |b| {
        b.iter(|| {
            fn fib(n: u64) -> u64 {
                if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
            }
            std::hint::black_box(fib(std::hint::black_box(20)))
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
EOF
cargo generate-lockfile
git add -A && git commit -qm "C: criterion fib 20"
git tag fixture-c

# commit D: criterion bench, same benchmark id with fib(21)
sed -i 's/black_box(20)/black_box(21)/g' benches/fixture_bench.rs
git add -A && git commit -qm "D: criterion fib 21"
git tag fixture-d

echo "fixture ready: $dir (tags fixture-a, fixture-b, fixture-c, fixture-d)"
