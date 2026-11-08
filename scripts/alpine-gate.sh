#!/bin/sh
# alpine-gate.sh - musl runtime gate for a V18-13 release build.
#
# Runs INSIDE an Alpine 3.21 container with the repository mounted at /work:
#   docker run --rm -v "$PWD:/work" -w /work alpine:3.21 sh scripts/alpine-gate.sh x86_64
#
# x86_64 builds the dynamic binary, the static fallback and all seven native
# plugins, then exercises the real runtime matrix (dynamic + 7 plugins,
# missing libgcc, missing libstdc++, static fallback, existing native plugin
# refusal, --static-fallback ignored while dynamic is usable). aarch64 is a
# build/linkage-only gate (QEMU is slow); its full runtime remains an explicit
# untested boundary in R3.11.16.
#
# This script is intentionally self-contained and uses only Alpine packages.

set -eu

ARCH=${1:-x86_64}
MODE=${2:-runtime}
case "$ARCH" in
    x86_64) TARGET=x86_64-unknown-linux-musl ;;
    aarch64) TARGET=aarch64-unknown-linux-musl ;;
    *) echo "usage: $0 x86_64|aarch64 [runtime|build-only]" >&2; exit 2 ;;
esac
case "$MODE" in
    runtime | build-only) ;;
    *) echo "usage: $0 x86_64|aarch64 [runtime|build-only]" >&2; exit 2 ;;
esac

EXT_CRATES=${EXT_CRATES:-"rpi-ext-subagents rpi-ext-smart-fetch rpi-ext-statusline rpi-ext-ask-user-question rpi-ext-todo rpi-ext-plan-mode rpi-ext-usage"}

echo "=== Alpine $(cat /etc/alpine-release) / $ARCH ($TARGET) ==="
apk add --no-cache \
    bash binutils build-base ca-certificates cmake curl file g++ git jq \
    linux-headers musl-dev perl pkgconf python3 tar xz zstd >/dev/null

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.98.1 --target "$TARGET" >/dev/null
fi
# shellcheck disable=SC1090
. "$HOME/.cargo/env"
rustc --version
cargo --version

export CARGO_TERM_COLOR=always
export CC=cc
export CXX=c++

echo "=== build dynamic $TARGET ==="
RUSTFLAGS="-C target-feature=-crt-static -C link-self-contained=no" \
    cargo build --release --target "$TARGET" --bin rpi
RUSTFLAGS="-C target-feature=-crt-static -C link-self-contained=no" \
    cargo build --release --target "$TARGET" $(printf ' -p %s' $EXT_CRATES)

DYNAMIC="target/$TARGET/release/rpi"
[ -x "$DYNAMIC" ] || { echo "missing dynamic binary: $DYNAMIC" >&2; exit 1; }
readelf -l "$DYNAMIC" | grep -q 'Requesting program interpreter' ||
    { echo "dynamic binary has no ELF interpreter" >&2; exit 1; }

echo "=== build static fallback ==="
CARGO_TARGET_DIR=target/static-fallback RPI_DIST_TARGET="$TARGET-static" \
    cargo build --release --target "$TARGET" --bin rpi
STATIC="target/static-fallback/$TARGET/release/rpi"
[ -x "$STATIC" ] || { echo "missing static binary: $STATIC" >&2; exit 1; }
if readelf -l "$STATIC" | grep -q 'Requesting program interpreter'; then
    echo "static fallback is dynamically linked" >&2
    exit 1
fi
file "$DYNAMIC" "$STATIC"

# Materialize the loader layout from the real cdylibs + manifests. The
# installer's .rpix path is covered by test-install-sh.sh; this gate proves the
# binaries themselves can load all seven native carriers on Alpine.
AGENT=/tmp/rpi-alpine-agent
rm -rf "$AGENT"
mkdir -p "$AGENT/extensions"
for crate in $EXT_CRATES; do
    manifest="crates/$crate/rpi-extension.json"
    name=$(jq -r .name "$manifest")
    native=$(jq -r .native "$manifest")
    lib="target/$TARGET/release/lib$(printf '%s' "$crate" | tr '-' '_').so"
    [ -f "$lib" ] || { echo "missing cdylib: $lib" >&2; exit 1; }
    mkdir -p "$AGENT/extensions/$name"
    cp "$manifest" "$AGENT/extensions/$name/rpi-extension.json"
    cp "$lib" "$AGENT/extensions/$name/$native"
done
echo "plugins: $(find "$AGENT/extensions" -mindepth 1 -maxdepth 1 -type d | wc -l)"

if [ "$MODE" = build-only ]; then
    echo "aarch64 build/linkage gate passed; full runtime remains an explicit untested boundary"
    exit 0
fi

get_commands() { # get_commands <agent-dir> <output-file>
    # A finite timeout protects the gate from an RPC read that never sees EOF.
    if command -v timeout >/dev/null 2>&1; then
        printf '%s\n' '{"type":"get_commands"}' |
            RPI_CODING_AGENT_DIR="$1" timeout 60 "$DYNAMIC" --mode rpc >"$2" 2>&1
    else
        printf '%s\n' '{"type":"get_commands"}' |
            RPI_CODING_AGENT_DIR="$1" "$DYNAMIC" --mode rpc >"$2" 2>&1
    fi
}

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "=== remove runtime libraries: dynamic must fail, static must run ==="
apk del --no-network build-base gcc g++ musl-dev cmake perl pkgconf linux-headers jq >/dev/null 2>&1 || true
apk del --no-network libgcc libstdc++ >/dev/null 2>&1 || true
if [ -e /usr/lib/libgcc_s.so.1 ]; then
    fail "libgcc_s.so.1 is still installed; cannot test the missing-library path"
fi
if "$DYNAMIC" --version >/tmp/alpine-dynamic-missing.out 2>&1; then
    fail "dynamic rpi ran without libgcc_s.so.1"
fi
grep -q 'libgcc' /tmp/alpine-dynamic-missing.out ||
    fail "missing-libgcc error does not name libgcc: $(cat /tmp/alpine-dynamic-missing.out)"
"$STATIC" --version >/tmp/alpine-static.out 2>&1 ||
    fail "static fallback does not run without runtime libraries: $(cat /tmp/alpine-static.out)"
grep -q "$TARGET-static\|rpi" /tmp/alpine-static.out || true

echo "=== add libgcc: dynamic base runs, smart-fetch still needs libstdc++ ==="
apk add --no-cache libgcc >/dev/null
"$DYNAMIC" --version >/tmp/alpine-dynamic.out 2>&1 ||
    fail "dynamic rpi does not run with libgcc: $(cat /tmp/alpine-dynamic.out)"

SMART_AGENT=/tmp/rpi-alpine-agent-smart
rm -rf "$SMART_AGENT"
mkdir -p "$SMART_AGENT/extensions"
cp -R "$AGENT/extensions/rpi-smart-fetch" "$SMART_AGENT/extensions/"
get_commands "$SMART_AGENT" /tmp/alpine-smart.out || true
grep -q 'libstdc++' /tmp/alpine-smart.out ||
    fail "smart-fetch did not report its missing libstdc++ dependency: $(cat /tmp/alpine-smart.out)"

echo "=== add libstdc++: dynamic + all seven plugins ==="
apk add --no-cache libstdc++ >/dev/null
get_commands "$AGENT" /tmp/alpine-commands.out ||
    fail "get_commands failed with all plugins: $(cat /tmp/alpine-commands.out)"
for token in todos usage plan statusline; do
    grep -q "$token" /tmp/alpine-commands.out ||
        fail "get_commands output is missing '$token': $(cat /tmp/alpine-commands.out)"
done

echo "=== static fallback refuses existing native plugins ==="
STATIC_AGENT=/tmp/rpi-alpine-static-agent
rm -rf "$STATIC_AGENT"
mkdir -p "$STATIC_AGENT/extensions/rpi-todo"
printf '%s\n' '{"name":"rpi-todo","native":"librpi_ext_todo.so"}' >"$STATIC_AGENT/extensions/rpi-todo/rpi-extension.json"
: >"$STATIC_AGENT/extensions/rpi-todo/librpi_ext_todo.so"
if (
    RPI_INSTALL_TEST_SOURCE=1 . ./install.sh
    HOME=/tmp RPI_CODING_AGENT_DIR="$STATIC_AGENT" precheck_existing_native_plugins
) >/tmp/alpine-static-refusal.out 2>&1; then
    fail "static fallback did not refuse an existing native plugin"
fi
grep -q 'existing native plugins block the static fallback' /tmp/alpine-static-refusal.out ||
    fail "static refusal message is missing: $(cat /tmp/alpine-static-refusal.out)"

echo "=== --static-fallback is ignored while dynamic is usable ==="
if ! (
    RPI_INSTALL_TEST_SOURCE=1 . ./install.sh
    IS_MUSL_TARGET=1
    PROFILE=standard
    STATIC_FLAG=1
    IS_STATIC=0
    RPI_INSTALL_HAS_LIBGCC=1
    RPI_INSTALL_HAS_LIBSTDCXX=1
    choose_static_fallback
    [ "$IS_STATIC" = 0 ]
) >/tmp/alpine-fallback.out 2>&1; then
    fail "dynamic runtime did not override --static-fallback: $(cat /tmp/alpine-fallback.out)"
fi
grep -q 'fallback is ignored' /tmp/alpine-fallback.out ||
    fail "ignored --static-fallback was not explained: $(cat /tmp/alpine-fallback.out)"

echo "Alpine $ARCH runtime gate passed"