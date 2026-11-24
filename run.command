#!/bin/zsh -l
cd "$(dirname "$0")"
echo "=== cargo build --release ==="
cargo build --release 2>&1 | tee build.log
if [ ${pipestatus[1]} -ne 0 ]; then echo "BUILD FAILED"; exit 1; fi
echo "=== devices ==="
./target/release/vinylizer --list-devices 2>&1 | tee run.log
echo "=== running (Ctrl-C to stop) ==="
./target/release/vinylizer 2>&1 | tee -a run.log
