#!/bin/sh
# test-install-sh.sh — self-contained tests for install.sh.
#
# Builds a fake GitHub release (dummy rpi binaries + .sha256 sidecars) in a
# temp dir and serves it over a local HTTP server (python3) or, failing that,
# via file:// URLs (requires curl). No real network access, no system changes:
# everything lives under a mktemp dir cleaned up on exit.
#
# Covered: sh -n / dash -n syntax; install with default prefix (HOME override)
# and with --prefix; sha256 mismatch aborts without residue; both version
# resolution paths (fake GitHub API via RPI_GITHUB_API_URL, endpoint fallback
# via RPI_VERSION_CHECK_URL + RPI_RELEASE_BASE_URL); --musl asset selection;
# non-writable prefix -> sudo hint branch; mirror fallback (GitHub candidate
# 404 -> RPI_SITE_BASE_URL succeeds; all candidates failing -> error exit).
# The fake server plays both GitHub and the site mirror: assets live under
# /releases/download/... only, so a GitHub base pointing elsewhere 404s.

set -eu

HERE=$(CDPATH= cd "$(dirname "$0")" && pwd)
INSTALL_SH=$HERE/../install.sh

[ -f "$INSTALL_SH" ] || { echo "install.sh not found at $INSTALL_SH" >&2; exit 1; }

# ---- tool prerequisites ------------------------------------------------------

if command -v sha256sum >/dev/null 2>&1; then
    SHA256TOOL=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    SHA256TOOL="shasum -a 256"
else
    echo "sha256sum/shasum missing; cannot build fake release" >&2
    exit 1
fi
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || {
    echo "curl/wget missing; install.sh cannot run" >&2
    exit 1
}
command -v tar >/dev/null 2>&1 || { echo "tar missing" >&2; exit 1; }

# ---- host target (mirrors install.sh detection) ------------------------------

host_arch=$(uname -m)
case $host_arch in
    x86_64 | amd64) host_arch=x86_64 ;;
    aarch64 | arm64) host_arch=aarch64 ;;
    *) echo "unsupported host arch: $host_arch" >&2; exit 1 ;;
esac
host_os=$(uname -s)
case $host_os in
    Linux)
        if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
            HOST_TARGET=$host_arch-unknown-linux-musl
        else
            HOST_TARGET=$host_arch-unknown-linux-gnu
        fi
        MUSL_TARGET=$host_arch-unknown-linux-musl
        ;;
    Darwin)
        [ "$host_arch" = aarch64 ] || { echo "unsupported host: $host_os $host_arch" >&2; exit 1; }
        HOST_TARGET=aarch64-apple-darwin
        MUSL_TARGET="" # no musl on macOS; --musl case skipped
        ;;
    *)
        echo "unsupported host OS: $host_os" >&2
        exit 1
        ;;
esac

# ---- workspace ----------------------------------------------------------------

TMP=$(mktemp -d)
SERVER_PID=""
cleanup() {
    [ -z "$SERVER_PID" ] || kill "$SERVER_PID" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

VERSION=0.1.0
WWW=$TMP/www

make_fake_archive() { # make_fake_archive <version> <target> [corrupt-sha]
    fv=$1
    ft=$2
    corrupt=${3:-}
    stage=$TMP/stage-$fv-$ft
    mkdir -p "$stage"
    printf '#!/bin/sh\necho "rpi fake %s %s"\n' "$fv" "$ft" >"$stage/rpi"
    chmod 755 "$stage/rpi"
    out=$WWW/releases/download/v$fv
    [ -z "$corrupt" ] || out=$WWW/bad-releases/download/v$fv
    mkdir -p "$out"
    tar -czf "$out/rpi-$fv-$ft.tar.gz" -C "$stage" rpi
    if [ -z "$corrupt" ]; then
        (cd "$out" && $SHA256TOOL "rpi-$fv-$ft.tar.gz" >"rpi-$fv-$ft.tar.gz.sha256")
    else
        printf '%064d  %s\n' 0 "rpi-$fv-$ft.tar.gz" >"$out/rpi-$fv-$ft.tar.gz.sha256"
    fi
}

# Assets for the good release: host target (gnu or musl as detected) + musl
# variant (for the --musl case) on Linux; darwin target on macOS.
make_fake_archive "$VERSION" "$HOST_TARGET"
if [ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ]; then
    make_fake_archive "$VERSION" "$MUSL_TARGET"
fi

# Broken release for the checksum-failure case (endpoint-fallback resolution).
BAD_VERSION=0.2.0
make_fake_archive "$BAD_VERSION" "$HOST_TARGET" corrupt
mkdir -p "$WWW/bad"
printf '{"version": "%s", "packageName": "rpi"}\n' "$BAD_VERSION" >"$WWW/bad/latest-version.json"

# Version endpoint document for the fallback path.
printf '{"version": "%s", "packageName": "rpi", "note": "test"}\n' "$VERSION" >"$WWW/latest-version.json"

# ---- serve the fake release ---------------------------------------------------

if command -v python3 >/dev/null 2>&1; then
    # -u: unbuffered, so the "Serving HTTP on ... port N" line hits the log
    (cd "$WWW" && exec python3 -u -m http.server 0 --bind 127.0.0.1) >"$TMP/server.log" 2>&1 &
    SERVER_PID=$!
    PORT=""
    i=0
    while [ $i -lt 50 ]; do
        PORT=$(sed -n 's/.*port \([0-9][0-9]*\).*/\1/p' "$TMP/server.log" 2>/dev/null | head -n 1)
        [ -n "$PORT" ] && break
        sleep 0.1
        i=$((i + 1))
    done
    [ -n "$PORT" ] || { echo "local HTTP server did not start:" >&2; cat "$TMP/server.log" >&2; exit 1; }
    BASE=http://127.0.0.1:$PORT
    SERVE_MODE="http (python3, port $PORT)"
elif command -v curl >/dev/null 2>&1; then
    BASE=file://$WWW
    SERVE_MODE="file://"
else
    echo "python3 and curl both missing; cannot serve the fake release" >&2
    exit 1
fi
echo "serving fake release via $SERVE_MODE"

# Fake GitHub API document (compact JSON; browser_download_url points at BASE).
assets_json=""
for t in "$HOST_TARGET" $([ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ] && echo "$MUSL_TARGET"); do
    [ -n "$t" ] || continue
    n=rpi-$VERSION-$t.tar.gz
    entry="{\"name\": \"$n\", \"browser_download_url\": \"$BASE/releases/download/v$VERSION/$n\"}"
    entry_sha="{\"name\": \"$n.sha256\", \"browser_download_url\": \"$BASE/releases/download/v$VERSION/$n.sha256\"}"
    if [ -z "$assets_json" ]; then
        assets_json="$entry,$entry_sha"
    else
        assets_json="$assets_json,$entry,$entry_sha"
    fi
done
printf '{"tag_name": "v%s", "assets": [%s]}\n' "$VERSION" "$assets_json" >"$WWW/api-latest.json"

API_URL=$BASE/api-latest.json
ENDPOINT_URL=$BASE/latest-version.json
DEAD_URL=$BASE/does-not-exist.json

# ---- tiny test harness ---------------------------------------------------------

TESTS_RUN=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); printf 'ok %d - %s\n' "$TESTS_RUN" "$1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); printf 'not ok %d - %s\n' "$TESTS_RUN" "$1"; }
skip() { printf 'ok - %s # SKIP\n' "$1"; }

check() { # check <description> <command...>
    desc=$1
    shift
    if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}

expect_ok() { # expect_ok <description> — runs remaining args, expects exit 0
    desc=$1
    shift
    if "$@" >"$TMP/last-out" 2>&1; then
        pass "$desc"
    else
        fail "$desc"
        sed 's/^/    | /' "$TMP/last-out"
    fi
}

expect_fail() { # expect_fail <description> — runs remaining args, expects non-zero
    desc=$1
    shift
    if "$@" >"$TMP/last-out" 2>&1; then
        fail "$desc (unexpectedly succeeded)"
        sed 's/^/    | /' "$TMP/last-out"
    else
        pass "$desc"
    fi
}

# ---- 1. syntax ----------------------------------------------------------------

expect_ok "sh -n syntax check" sh -n "$INSTALL_SH"
if command -v dash >/dev/null 2>&1; then
    expect_ok "dash -n syntax check" dash -n "$INSTALL_SH"
else
    skip "dash -n syntax check (dash not installed)"
fi

# ---- 2. default prefix, GitHub API resolution path ------------------------------

H1=$TMP/home1
expect_ok "install via GitHub API, default prefix exits 0" \
    env HOME="$H1" RPI_GITHUB_API_URL="$API_URL" \
    RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
    sh "$INSTALL_SH"
check "binary installed at ~/.local/bin/rpi" test -x "$H1/.local/bin/rpi"
check "manifest written next to binary" test -f "$H1/.local/bin/rpi.install.json"
check "manifest target is host triple ($HOST_TARGET)" \
    grep -q "\"target\": \"$HOST_TARGET\"" "$H1/.local/bin/rpi.install.json"
check "manifest version matches" grep -q "\"version\": \"$VERSION\"" "$H1/.local/bin/rpi.install.json"
check "manifest method is binary" grep -q '"method": "binary"' "$H1/.local/bin/rpi.install.json"
check "manifest installPath is absolute" \
    grep -q "\"installPath\": \"$H1/.local/bin/rpi\"" "$H1/.local/bin/rpi.install.json"
check "output prints update guidance" grep -q 'rpi update --self' "$TMP/last-out"
check "output prints uninstall guidance" grep -q 'rpi self-uninstall' "$TMP/last-out"
check "output prints PATH hint for default prefix" grep -q 'not in your PATH' "$TMP/last-out"
check "installed binary runs" "$H1/.local/bin/rpi"

# ---- 3. --prefix override --------------------------------------------------------

P2=$TMP/prefix2
expect_ok "install with --prefix exits 0" \
    env HOME="$TMP/home2" RPI_GITHUB_API_URL="$API_URL" \
    RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
    sh "$INSTALL_SH" --prefix "$P2"
check "binary installed at custom prefix" test -x "$P2/rpi"
check "manifest installPath matches custom prefix" \
    grep -q "\"installPath\": \"$P2/rpi\"" "$P2/rpi.install.json"

# ---- 4. endpoint fallback resolution path ----------------------------------------

H4=$TMP/home4
expect_ok "install via endpoint fallback (API down) exits 0" \
    env HOME="$H4" RPI_GITHUB_API_URL="$DEAD_URL" \
    RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
    sh "$INSTALL_SH"
check "fallback installed binary" test -x "$H4/.local/bin/rpi"
check "fallback manifest version matches endpoint version" \
    grep -q "\"version\": \"$VERSION\"" "$H4/.local/bin/rpi.install.json"
check "fallback sourceUrl constructed from RPI_RELEASE_BASE_URL" \
    grep -q "\"sourceUrl\": \"$BASE/releases/download/v$VERSION/rpi-$VERSION-$HOST_TARGET.tar.gz\"" \
    "$H4/.local/bin/rpi.install.json"
check "fallback announced in output" grep -q 'falling back to version endpoint' "$TMP/last-out"

# ---- 5. sha256 mismatch aborts without residue -----------------------------------

H5=$TMP/home5
expect_fail "sha256 mismatch aborts with non-zero exit" \
    env HOME="$H5" RPI_GITHUB_API_URL="$DEAD_URL" \
    RPI_VERSION_CHECK_URL="$BASE/bad/latest-version.json" \
    RPI_RELEASE_BASE_URL="$BASE/bad-releases" \
    sh "$INSTALL_SH"
check "checksum failure reported" grep -q 'integrity check failed' "$TMP/last-out"
check "no binary left behind" test ! -e "$H5/.local/bin/rpi"
check "no manifest left behind" test ! -e "$H5/.local/bin/rpi.install.json"

# ---- 6. --musl asset selection -----------------------------------------------------

if [ "$host_os" = Linux ]; then
    H6=$TMP/home6
    expect_ok "install with --musl exits 0" \
        env HOME="$H6" RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --musl
    check "--musl picks the musl asset ($MUSL_TARGET)" \
        grep -q "\"target\": \"$MUSL_TARGET\"" "$H6/.local/bin/rpi.install.json"
    check "--musl sourceUrl names the musl archive" \
        grep -q "rpi-$VERSION-$MUSL_TARGET.tar.gz" "$H6/.local/bin/rpi.install.json"
else
    skip "--musl asset selection (Linux-only)"
fi

# ---- 7. non-writable prefix -> sudo hint -------------------------------------------

if [ "$(id -u)" = 0 ]; then
    skip "non-writable prefix sudo hint (running as root)"
else
    RO=$TMP/ro-prefix
    mkdir -p "$RO"
    chmod 555 "$RO"
    expect_fail "non-writable prefix exits non-zero" \
        env HOME="$TMP/home7" RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$RO"
    check "sudo hint printed" grep -qi 'sudo' "$TMP/last-out"
    check "not-writable reason printed" grep -q 'not writable' "$TMP/last-out"
    chmod 755 "$RO"
fi

# ---- 8. GitHub candidate 404 -> site mirror fallback ---------------------------

# RPI_RELEASE_BASE_URL points at a path with no assets (GitHub 404); the
# fake server doubles as the site mirror (assets under /releases/download).
H8=$TMP/home8
expect_ok "GitHub asset 404 falls back to site mirror, exits 0" \
    env HOME="$H8" RPI_GITHUB_API_URL="$DEAD_URL" \
    RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
    RPI_RELEASE_BASE_URL="$BASE/gh-releases" \
    RPI_SITE_BASE_URL="$BASE" \
    sh "$INSTALL_SH"
check "mirror fallback installed binary" test -x "$H8/.local/bin/rpi"
check "mirror fallback sourceUrl is the site mirror" \
    grep -q "\"sourceUrl\": \"$BASE/releases/download/v$VERSION/rpi-$VERSION-$HOST_TARGET.tar.gz\"" \
    "$H8/.local/bin/rpi.install.json"
check "mirror fallback announced in output" grep -q 'trying the next mirror' "$TMP/last-out"

# ---- 9. all download candidates fail -> error exit -------------------------------

H9=$TMP/home9
expect_fail "site mirror also down: install exits non-zero" \
    env HOME="$H9" RPI_GITHUB_API_URL="$DEAD_URL" \
    RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
    RPI_RELEASE_BASE_URL="$BASE/gh-releases" \
    RPI_SITE_BASE_URL="$BASE/no-site" \
    sh "$INSTALL_SH"
check "all-candidates-failed error printed" \
    grep -q 'all download candidates failed' "$TMP/last-out"
check "no binary left behind (mirror case)" test ! -e "$H9/.local/bin/rpi"
check "no manifest left behind (mirror case)" test ! -e "$H9/.local/bin/rpi.install.json"

# ---- summary ------------------------------------------------------------------------

echo
echo "ran $TESTS_RUN checks, $TESTS_FAILED failed"
[ "$TESTS_FAILED" -eq 0 ]
