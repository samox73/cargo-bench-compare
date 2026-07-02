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
echo "fixture ready: $dir (tags fixture-a, fixture-b)"
