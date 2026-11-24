#!/bin/zsh -l
cd "$(dirname "$0")"
cargo build --release 2>&1 | tail -3
echo "=== AU effects ==="
./target/release/vinylizer --list-plugins 2>&1 | grep -i -E "vinyl|izotope"
echo "=== running with iZotope Vinyl (Ctrl-C to stop) ==="
./target/release/vinylizer --au Vinyl 2>&1 | tee run-izotope.log
