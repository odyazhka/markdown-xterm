#!/bin/sh
# One-shot build: Rust library -> download xterm-411 -> patch -> build xterm.
# Result: build/xterm-411/xterm
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
WORK="$HERE/build"
LIBDIR="$HERE/target/release"
URL="https://github.com/ThomasDickey/xterm-snapshots/archive/refs/tags/xterm-411.tar.gz"

echo "==> [1/4] Building the Rust library"
(cd "$HERE" && cargo build --release)

echo "==> [2/4] Fetching xterm-411"
mkdir -p "$WORK"
if [ ! -d "$WORK/xterm-411" ]; then
    curl -fsSL "$URL" | tar -xz -C "$WORK"
    mv "$WORK/xterm-snapshots-xterm-411" "$WORK/xterm-411"
fi
cd "$WORK/xterm-411"

echo "==> [3/4] Applying the patch"
cp "$HERE/mdterm_bridge.h" .
if grep -q mdterm_hook_flush_due ptydata.c 2>/dev/null; then
    echo "    (already patched, skipping)"
else
    patch -p1 < "$HERE/xterm-411-mdterm.patch"
fi

echo "==> [4/4] Building xterm"
LIBS="-L$LIBDIR -lmdterm_bridge -lpthread -ldl -lm" ./configure >/dev/null
rm -f xterm            # make does not notice a changed .a, force a relink
make -j"$(nproc 2>/dev/null || echo 2)" >/dev/null

echo
echo "Done: $WORK/xterm-411/xterm"
echo "Try:  $WORK/xterm-411/xterm -e sh $HERE/examples/demo.sh"
