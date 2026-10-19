#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/repl.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/repl" ;;
    *) rel="x86_64-apple-darwin/repl" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/repl" ;;
    *) rel="x86_64-unknown-linux-musl/repl" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/repl.exe" "$here/target/release/repl" "$here/target/debug/repl.exe" "$here/target/debug/repl"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "repl binary not found in $here" >&2
exit 1
