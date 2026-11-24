#!/bin/zsh -l
# Build Vinylizer.app (release, LTO, native CPU), install to ~/Applications and launch.
cd "$(dirname "$0")"
exec > >(tee build.log) 2>&1
pkill -f 'target/release/vinylizer' 2>/dev/null
pkill -x Vinylizer 2>/dev/null
cargo build --release || { echo "BUILD FAILED"; exit 1; }
APP="$HOME/Applications/Vinylizer.app"
sleep 1
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/vinylizer "$APP/Contents/MacOS/Vinylizer"
cp Info.plist "$APP/Contents/Info.plist"
codesign --force -s - "$APP" && echo "signed"
open "$APP" && echo "LAUNCHED $APP"
