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
# 404 -> RPI_SITE_BASE_URL succeeds; all candidates failing -> error exit);
# PATH handling (--add-path writes a marked shell-profile block, --no-add-path
# does not, writes are idempotent, zsh/fish/custom-prefix detection,
# symlink/mode preservation across prefix changes, HOME trailing slash,
# special-character paths, legacy absolute lines, mixed-case env,
# mutual-exclusion flags, unsupported shells, bash .bash_profile, and the
# interactive pty prompt plus its read -t timeout).
# The fake server plays both GitHub and the site mirror: assets live under
# /releases/download/... only, so a GitHub base pointing elsewhere 404s.

set -eu

# Keep the installer non-interactive by default; individual PATH tests below
# override this with --add-path or RPI_INSTALL_ADD_PATH=1. Clear the shell
# profile overrides so tests never touch a developer's real dotfiles.
export RPI_INSTALL_ADD_PATH=0
# The legacy checks exercise the mini (binary-only) install path; the profile
# matrix at the end explicitly overrides this for standard/full/static runs.
export RPI_INSTALL_PROFILE=mini
unset ZDOTDIR XDG_CONFIG_HOME RPI_NONINTERACTIVE RPI_INSTALL_PROMPT_TIMEOUT
unset RPI_INSTALL_YES RPI_INSTALL_STATIC_FALLBACK RPI_CODING_AGENT_DIR
unset RPI_INSTALL_CACHE_DIR RPI_INSTALL_REQUIRE_PLUGINS RPI_INSTALL_NO_CACHE

# The default profile block is an idempotent case guard (one line).
DEFAULT_PATH_LINE='case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) export PATH="$HOME/.local/bin:$PATH" ;; esac'

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

# Portable file-mode read: GNU stat first, BSD stat second.
file_mode() {
    if stat -c %a "$1" >/dev/null 2>&1; then
        stat -c %a "$1"
    else
        stat -f %Lp "$1"
    fi
}

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
    # Pad the fake binary to ~40 KiB so a stalled transfer produces a
    # meaningful partial body (curl flushes small buffers only at EOF, which
    # would make the resume/interrupt tests vacuous).
    {
        printf '#!/bin/sh\necho "rpi fake %s %s"\n' "$fv" "$ft"
        _pad=0
        while [ "$_pad" -lt 1200 ]; do
            printf '# padding line %s for resumable-download tests\n' "$_pad"
            _pad=$((_pad + 1))
        done
        printf 'exit 0\n'
    } >"$stage/rpi"
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

# Profile plugin lists: prefer scripts/profiles.json (the packaging single
# source of truth), fall back to the documented order without python3.
if command -v python3 >/dev/null 2>&1; then
    STANDARD_PLUGINS=$(python3 -c 'import json,sys; print(" ".join(json.load(open(sys.argv[1]))["profiles"]["standard"]))' "$HERE/profiles.json")
    FULL_PLUGINS=$(python3 -c 'import json,sys; print(" ".join(json.load(open(sys.argv[1]))["profiles"]["full"]))' "$HERE/profiles.json")
else
    STANDARD_PLUGINS='rpi-subagents rpi-todo rpi-ask-user-question rpi-smart-fetch rpi-statusline rpi-plan-mode'
    FULL_PLUGINS="$STANDARD_PLUGINS rpi-usage"
fi

# make_fake_rpix <outdir> <name> <version> <target> [minhost] [abi] [mode]
# mode: ok | escape | extra | badsum | missingentry
make_fake_rpix() {
    rp_out=$1 rp_name=$2 rp_ver=$3 rp_tgt=$4 rp_min=${5:-} rp_abi=${6:-1} rp_mode=${7:-ok}
    rp_stage=$TMP/rpix-stage-$rp_name-$rp_mode
    rm -rf "$rp_stage"
    mkdir -p "$rp_stage"
    {
        printf '{\n  "name": "%s",\n  "version": "%s",\n  "native": "libtest.so",\n  "rpiAbi": %s' \
            "$rp_name" "$rp_ver" "$rp_abi"
        if [ -n "$rp_min" ]; then
            printf ',\n  "minHostVersion": "%s"' "$rp_min"
        fi
        if [ "$rp_name" = rpi-statusline ]; then
            printf ',\n  "defaultSettings": {\n    "statusLine": {"type": "items", "items": ["live-total-tokens"]}\n  }'
        fi
        printf '\n}\n'
    } >"$rp_stage/rpi-extension.json"
    printf 'fake cdylib for %s\n' "$rp_name" >"$rp_stage/libtest.so"
    case $rp_mode in
        escape)
            ( cd "$rp_stage" && $SHA256TOOL rpi-extension.json libtest.so > SHA256SUMS )
            python3 - "$rp_stage" "$TMP/rpix-escape.tgz" <<'PYEOF'
import os, sys, tarfile
stage, out = sys.argv[1], sys.argv[2]
with tarfile.open(out, "w:gz") as archive:
    for name in ("rpi-extension.json", "libtest.so", "SHA256SUMS"):
        archive.add(os.path.join(stage, name), arcname=name)
    payload = b"escape\n"
    info = tarfile.TarInfo("../evil")
    info.size = len(payload)
    import io
    archive.addfile(info, io.BytesIO(payload))
PYEOF
            mkdir -p "$rp_out"
            cp "$TMP/rpix-escape.tgz" "$rp_out/$rp_name-$rp_ver-$rp_tgt.rpix"
            ( cd "$rp_out" && $SHA256TOOL "$rp_name-$rp_ver-$rp_tgt.rpix" >"$rp_name-$rp_ver-$rp_tgt.rpix.sha256" )
            return 0
            ;;
        extra)
            printf 'unlisted\n' >"$rp_stage/extra.txt"
            ;;
    esac
    case $rp_mode in
        badsum)
            printf '%064d  rpi-extension.json\n' 0 >"$rp_stage/SHA256SUMS"
            ;;
        missingentry)
            ( cd "$rp_stage" && $SHA256TOOL rpi-extension.json > SHA256SUMS )
            ;;
        extra)
            ( cd "$rp_stage" && $SHA256TOOL rpi-extension.json libtest.so > SHA256SUMS )
            ;;
        *)
            ( cd "$rp_stage" && $SHA256TOOL rpi-extension.json libtest.so > SHA256SUMS )
            ;;
    esac
    mkdir -p "$rp_out"
    # Bare filenames (no ./ prefix) like the release workflow's packaging.
    ( cd "$rp_stage" && tar -czf "$rp_out/$rp_name-$rp_ver-$rp_tgt.rpix" $(ls) )
    ( cd "$rp_out" && $SHA256TOOL "$rp_name-$rp_ver-$rp_tgt.rpix" >"$rp_name-$rp_ver-$rp_tgt.rpix.sha256" )
}

# make_fake_bundle <release-root> <version> <target> <profile> [mode]
# mode applies to the first plugin: ok | bad-plugin-sidecar | escape | extra |
# badsum | missingentry | abi2 | minhost-new | minhost-pre | minhost-bad
make_fake_bundle() {
    _root=$1 _ver=$2 _tgt=$3 _profile=$4 _mode=${5:-ok}
    _stage=$TMP/bundle-$_ver-$_tgt-$_profile-$_mode
    rm -rf "$_stage"
    mkdir -p "$_stage/extensions"
    printf '#!/bin/sh\necho "rpi fake %s %s %s"\n' "$_ver" "$_tgt" "$_profile" >"$_stage/rpi"
    chmod 755 "$_stage/rpi"
    if [ "$_profile" = full ]; then
        _plugins=$FULL_PLUGINS
    else
        _plugins=$STANDARD_PLUGINS
    fi
    _first=$STANDARD_PLUGINS
    _first=${_first%% *}
    for _p in $_plugins; do
        _min=""
        _abi=1
        _pmode=ok
        if [ "$_p" = "$_first" ]; then
            case $_mode in
                bad-plugin-sidecar) _pmode=ok ;;
                escape | extra | badsum | missingentry) _pmode=$_mode ;;
                abi2) _abi=2 ;;
                minhost-new) _min=0.1.999 ;;
                minhost-pre) _min=0.0.0-rc.1 ;;
                minhost-bad) _min=not-a-version ;;
            esac
        fi
        make_fake_rpix "$_stage/extensions" "$_p" "$_ver" "$_tgt" "$_min" "$_abi" "$_pmode"
        if [ "$_p" = "$_first" ] && [ "$_mode" = bad-plugin-sidecar ]; then
            printf '%064d  %s-%s-%s.rpix\n' 0 "$_p" "$_ver" "$_tgt" \
                >"$_stage/extensions/$_p-$_ver-$_tgt.rpix.sha256"
        fi
    done
    {
        printf '{\n  "packages": ['
        _sep=""
        for _p in $_plugins; do
            printf '%s\n    "%s"' "$_sep" "$_p"
            _sep=","
        done
        printf '\n  ]'
        if echo " $_plugins " | grep -q ' rpi-statusline '; then
            printf ',\n  "statusLine": {\n    "type": "items",\n    "items": ["live-total-tokens"]\n  }'
        fi
        printf '\n}\n'
    } >"$_stage/settings.defaults.json"
    _out=$_root/releases/download/v$_ver
    mkdir -p "$_out"
    rm -f "$_out/rpi-$_ver-$_tgt-$_profile.tar.gz"*
    tar -czf "$_out/rpi-$_ver-$_tgt-$_profile.tar.gz" -C "$_stage" rpi settings.defaults.json extensions
    ( cd "$_out" && $SHA256TOOL "rpi-$_ver-$_tgt-$_profile.tar.gz" \
        >"rpi-$_ver-$_tgt-$_profile.tar.gz.sha256" )
}

# make_unsafe_bundle <release-root> <version> <target> <profile>
make_unsafe_bundle() {
    _root=$1 _ver=$2 _tgt=$3 _profile=$4
    _stage=$TMP/unsafe-bundle-$_ver-$_tgt
    rm -rf "$_stage"
    mkdir -p "$_stage"
    printf '#!/bin/sh\necho unsafe\n' >"$_stage/rpi"
    chmod 755 "$_stage/rpi"
    printf 'escape\n' >"$_stage/evil"
    _out=$_root/releases/download/v$_ver
    mkdir -p "$_out"
    # GNU tar refuses to append to a compressed archive, so use Python's
    # tarfile to emit a deliberately unsafe member name.
    python3 - "$_out/rpi-$_ver-$_tgt-$_profile.tar.gz" "$_stage" <<'PYEOF'
import sys, tarfile, os
out, stage = sys.argv[1], sys.argv[2]
with tarfile.open(out, "w:gz") as archive:
    archive.add(os.path.join(stage, "rpi"), arcname="rpi")
    archive.add(os.path.join(stage, "evil"), arcname="../evil")
PYEOF
    ( cd "$_out" && $SHA256TOOL "rpi-$_ver-$_tgt-$_profile.tar.gz" \
        >"rpi-$_ver-$_tgt-$_profile.tar.gz.sha256" )
}

# write_profile_index <file> <version> <asset> <sha> [<asset> <sha> ...]
write_profile_index() {
    _file=$1 _ver=$2
    shift 2
    mkdir -p "${_file%/*}"
    {
        printf '{\n  "schemaVersion": 1,\n  "version": "%s",\n  "assets": {\n' "$_ver"
        _sep=""
        while [ $# -ge 2 ]; do
            printf '%s    "%s": "%s"' "$_sep" "$1" "$2"
            _sep=",
"
            shift 2
        done
        printf '\n  }\n}\n'
    } >"$_file"
}

sha_of() { # sha_of <dir> <basename>
    ( cd "$1" && $SHA256TOOL "$2" | sed 's/[[:space:]].*//' )
}

# Assets for the good release: host target (gnu or musl as detected) + musl
# variant (for the --musl case) on Linux; darwin target on macOS.
make_fake_archive "$VERSION" "$HOST_TARGET"
if [ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ]; then
    make_fake_archive "$VERSION" "$MUSL_TARGET"
fi

# Dynamic profile bundles and the musl static fallback.
make_fake_bundle "$WWW" "$VERSION" "$HOST_TARGET" standard
make_fake_bundle "$WWW" "$VERSION" "$HOST_TARGET" full
if [ "$host_os" = Linux ]; then
    make_fake_bundle "$WWW" "$VERSION" "$MUSL_TARGET" standard
    make_fake_bundle "$WWW" "$VERSION" "$MUSL_TARGET" full
    _static_stage=$TMP/static-stage
    mkdir -p "$_static_stage"
    printf '#!/bin/sh\necho "rpi fake static"\n' >"$_static_stage/rpi"
    chmod 755 "$_static_stage/rpi"
    tar -czf "$WWW/releases/download/v$VERSION/rpi-$VERSION-$MUSL_TARGET-static.tar.gz" \
        -C "$_static_stage" rpi
    ( cd "$WWW/releases/download/v$VERSION" && \
        $SHA256TOOL "rpi-$VERSION-$MUSL_TARGET-static.tar.gz" \
        >"rpi-$VERSION-$MUSL_TARGET-static.tar.gz.sha256" )
fi

# Healthy rpi-pages profile index for the fake release.
_index_dir=$WWW/api/install-profiles
_index_args=""
for _t in "$HOST_TARGET" $([ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ] && echo "$MUSL_TARGET"); do
    [ -n "$_t" ] || continue
    for _prof in standard full; do
        _a=rpi-$VERSION-$_t-$_prof.tar.gz
        _s=$(sha_of "$WWW/releases/download/v$VERSION" "$_a")
        _index_args="$_index_args $_a $_s"
    done
done
write_profile_index "$_index_dir/$VERSION.json" "$VERSION" $_index_args

# Broken release for the checksum-failure case (endpoint-fallback resolution).
BAD_VERSION=0.2.0
make_fake_archive "$BAD_VERSION" "$HOST_TARGET" corrupt
mkdir -p "$WWW/bad"
printf '{"version": "%s", "packageName": "rpi"}\n' "$BAD_VERSION" >"$WWW/bad/latest-version.json"

# Version endpoint document for the fallback path.
printf '{"version": "%s", "packageName": "rpi", "note": "test"}\n' "$VERSION" >"$WWW/latest-version.json"

# ---- serve the fake release ---------------------------------------------------

cat >"$TMP/serve.py" <<'PYEOF'
# Test HTTP server: plain static files plus Range/If-Range support so the
# installer's resume path can be exercised end to end. 206 is returned only
# when the Range request carries an If-Range matching the current ETag (or no
# If-Range); a stale validator deliberately gets a full 200, mirroring a
# re-pushed release asset.
import email.utils
import hashlib
import http.server
import os
import re
import sys
import time


class Handler(http.server.SimpleHTTPRequestHandler):
    def send_file(self, head_only):
        path = self.translate_path(self.path)
        if not os.path.isfile(path):
            self.send_error(404)
            return
        with open(path, "rb") as handle:
            data = handle.read()
        etag = '"%s"' % hashlib.sha256(data).hexdigest()[:32]
        last_modified = email.utils.formatdate(os.path.getmtime(path), usegmt=True)
        status = 200
        start, end = 0, len(data) - 1
        range_header = self.headers.get("Range")
        if_range = self.headers.get("If-Range")
        if range_header and (if_range is None or if_range == etag):
            match = re.match(r"bytes=(\d+)-(\d*)$", range_header)
            if match:
                start = int(match.group(1))
                end = int(match.group(2)) if match.group(2) else len(data) - 1
                if start >= len(data):
                    self.send_response(416)
                    self.send_header("Content-Range", "bytes */%d" % len(data))
                    self.send_header("Cache-Control", "no-store")
                    self.end_headers()
                    return
                end = min(end, len(data) - 1)
                status = 206
        body = data[start : end + 1]
        omit_validators = False
        no_validator_marker = os.environ.get("RPI_TEST_206_NO_VALIDATOR_MARKER")
        if (
            range_header
            and no_validator_marker
            and self.path.endswith(".tar.gz")
            and os.path.exists(no_validator_marker)
        ):
            os.unlink(no_validator_marker)
            omit_validators = True
            status = 206
            # A wrong entity makes an append fail the final sha256 check; the
            # installer must discard the partial because the 206 carries no
            # validator to prove it is the same entity.
            body = b"X" * len(body)
        truncate = False
        stall = False
        stall_marker = os.environ.get("RPI_TEST_STALL_MARKER")
        # One-shot retryable failures: 500 (ordinary retry) and 429 with an
        # unparseable Retry-After (must fall back to jittered exponential
        # backoff instead of a fixed delay).
        fail_status = 0
        fail_retry_after = None
        for marker_name, code, retry_after in (
            ("RPI_TEST_500_MARKER", 500, None),
            ("RPI_TEST_429_BAD_MARKER", 429, "not-a-date"),
        ):
            marker_path = os.environ.get(marker_name)
            if (
                marker_path
                and self.path.endswith(".tar.gz")
                and os.path.exists(marker_path)
            ):
                os.unlink(marker_path)
                fail_status = code
                fail_retry_after = retry_after
                break
        if fail_status:
            self.send_response(fail_status)
            if fail_retry_after:
                self.send_header("Retry-After", fail_retry_after)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if (
            stall_marker
            and self.path.endswith(".tar.gz")
            and os.path.exists(stall_marker)
        ):
            os.unlink(stall_marker)
            stall = True
        marker = os.environ.get("RPI_TEST_TRUNCATE_MARKER")
        if (
            marker
            and self.path.endswith(".tar.gz")
            and os.path.exists(marker)
        ):
            os.unlink(marker)
            truncate = True
        self.send_response(status)
        if path.endswith(".sha256"):
            self.send_header("Content-Type", "text/plain; charset=utf-8")
        else:
            self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        if not omit_validators:
            self.send_header("ETag", etag)
            self.send_header("Last-Modified", last_modified)
        self.send_header("Accept-Ranges", "bytes")
        if status == 206:
            self.send_header("Content-Range", "bytes %d-%d/%d" % (start, end, len(data)))
            self.send_header("Cache-Control", "no-store")
        else:
            self.send_header("Cache-Control", "public, max-age=3600")
        self.end_headers()
        if not head_only:
            if stall:
                # Send headers and only part of the body, then hold the
                # connection so the installer's parent loop has to persist
                # validators while the transfer is still running.
                prefix = max(1, len(body) // 2)
                self.wfile.write(body[:prefix])
                self.wfile.flush()
                time.sleep(30)
                return
            if truncate:
                # Advertise the full length, then close after half the body so
                # the client retries and resumes with Range + If-Range.
                self.wfile.write(body[: max(1, len(body) // 2)])
                self.wfile.flush()
                self.close_connection = True
            else:
                self.wfile.write(body)

    def do_GET(self):
        self.send_file(False)

    def do_HEAD(self):
        self.send_file(True)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print("Serving HTTP on 127.0.0.1 port %d ..." % server.server_address[1], flush=True)
server.serve_forever()
PYEOF

if command -v python3 >/dev/null 2>&1; then
    # -u: unbuffered, so the "Serving HTTP on ... port N" line hits the log
    (cd "$WWW" && RPI_TEST_TRUNCATE_MARKER="$TMP/truncate-once" RPI_TEST_STALL_MARKER="$TMP/stall-once" RPI_TEST_206_NO_VALIDATOR_MARKER="$TMP/no-validator-once" RPI_TEST_500_MARKER="$TMP/fail-500-once" RPI_TEST_429_BAD_MARKER="$TMP/fail-429-once" exec python3 -u "$TMP/serve.py") >"$TMP/server.log" 2>&1 &
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
add_api_asset() {
    _n=$1
    _size=19000000
    case $_n in
        *-full.*) _size=34000000 ;;
        *-standard.*) _size=33000000 ;;
    esac
    _entry="{\"name\": \"$_n\", \"size\": $_size, \"browser_download_url\": \"$BASE/releases/download/v$VERSION/$_n\"}"
    _entry_sha="{\"name\": \"$_n.sha256\", \"browser_download_url\": \"$BASE/releases/download/v$VERSION/$_n.sha256\"}"
    if [ -z "$assets_json" ]; then
        assets_json="$_entry,$_entry_sha"
    else
        assets_json="$assets_json,$_entry,$_entry_sha"
    fi
}
for t in "$HOST_TARGET" $([ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ] && echo "$MUSL_TARGET"); do
    [ -n "$t" ] || continue
    add_api_asset "rpi-$VERSION-$t.tar.gz"
    add_api_asset "rpi-$VERSION-$t-standard.tar.gz"
    add_api_asset "rpi-$VERSION-$t-full.tar.gz"
    if [ "$host_os" = Linux ]; then
        add_api_asset "rpi-$VERSION-$t-static.tar.gz"
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
check "output prints PATH hint for default prefix" grep -q 'not the active rpi on PATH' "$TMP/last-out"
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

if [ "$host_os" = Linux ] && [ "$MUSL_TARGET" = "$HOST_TARGET" ]; then
    # musl host: the dynamic musl binary can run; force the dependency probes
    # so the test does not depend on the CI image's apk packages.
    H6=$TMP/home6
    expect_ok "install with --musl exits 0 (musl host, dynamic)" \
        env HOME="$H6" RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        RPI_INSTALL_HAS_LIBGCC=1 RPI_INSTALL_HAS_LIBSTDCXX=1 \
        sh "$INSTALL_SH" --musl
    check "--musl picks the musl asset ($MUSL_TARGET)" \
        grep -q "\"target\": \"$MUSL_TARGET\"" "$H6/.local/bin/rpi.install.json"
    check "--musl sourceUrl names the musl archive" \
        grep -q "rpi-$VERSION-$MUSL_TARGET.tar.gz" "$H6/.local/bin/rpi.install.json"
elif [ "$host_os" = Linux ]; then
    # glibc host: the dynamic musl runtime is unavailable, so --musl must only
    # proceed with explicit static-fallback consent.
    H6=$TMP/home6
    expect_fail "--musl on glibc without consent fails" \
        env HOME="$H6" RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --musl
    check "musl runtime guidance is printed" grep -q 'cannot run on this system' "$TMP/last-out"
    check "static consent guidance is printed" grep -q -- '--static-fallback' "$TMP/last-out"
    check "no binary after the refused static fallback" test ! -e "$H6/.local/bin/rpi"

    H6B=$TMP/home6b
    expect_ok "--musl --static-fallback installs the static asset" \
        env HOME="$H6B" RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --musl --static-fallback
    check "static manifest uses the -static label" \
        grep -q "\"target\": \"$MUSL_TARGET-static\"" "$H6B/.local/bin/rpi.install.json"
    check "static sourceUrl names the static archive" \
        grep -q "rpi-$VERSION-$MUSL_TARGET-static.tar.gz" "$H6B/.local/bin/rpi.install.json"
    check "static install writes no settings" test ! -e "$H6B/.rpi/agent/settings.json"
    check "static install summary says it cannot load plugins" \
        grep -q 'cannot load native plugins' "$TMP/last-out"
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

# ---- 10. PATH integration -------------------------------------------------------

if [ "$(id -u)" = 0 ]; then
    skip "PATH profile tests (running as root; installer refuses to edit profiles)"
else
    H10=$TMP/home10
    expect_ok "install --add-path writes the default shell profile" \
        env HOME="$H10" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "profile has the rpi marker" grep -Fxq '# rpi' "$H10/.profile"
    check "profile has the default PATH line" \
        grep -Fxq "$DEFAULT_PATH_LINE" "$H10/.profile"
    check "output reports the added profile" grep -q "Added $H10/.local/bin" "$TMP/last-out"

    expect_ok "second --add-path is idempotent" \
        env HOME="$H10" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "exactly one rpi marker remains" \
        test "$(grep -Fxc '# rpi' "$H10/.profile")" -eq 1

    H11=$TMP/home11
    expect_ok "zsh profile is detected via SHELL" \
        env HOME="$H11" SHELL=/bin/zsh RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "zsh PATH line written to .zshrc" \
        grep -Fxq "$DEFAULT_PATH_LINE" "$H11/.zshrc"

    H12=$TMP/home12
    CUSTOM_BIN=$H12/tools/rpi
    expect_ok "custom prefix under HOME writes an absolute PATH line" \
        env HOME="$H12" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$CUSTOM_BIN" --add-path
    CUSTOM_PATH_LINE=$(printf 'case ":$PATH:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac' "$CUSTOM_BIN" "$CUSTOM_BIN")
    check "custom prefix PATH line in profile" \
        grep -Fxq "$CUSTOM_PATH_LINE" "$H12/.profile"

    H13=$TMP/home13
    OUTSIDE_BIN=$TMP/outside-bin
    expect_ok "outside-HOME prefix with --add-path falls back to guidance" \
        env HOME="$H13" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$OUTSIDE_BIN" --add-path
    check "no profile written for an outside-HOME prefix" test ! -e "$H13/.profile"
    check "outside-HOME install prints PATH guidance" grep -q 'not the active rpi on PATH' "$TMP/last-out"
    check "outside-HOME install explains the refusal" \
        grep -q 'not modifying a shell profile' "$TMP/last-out"

    H14=$TMP/home14
    expect_ok "RPI_INSTALL_ADD_PATH=1 writes without a flag" \
        env HOME="$H14" SHELL=/bin/bash RPI_INSTALL_ADD_PATH=1 \
        RPI_GITHUB_API_URL="$API_URL" RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
        RPI_RELEASE_BASE_URL="$BASE/releases" sh "$INSTALL_SH"
    check "env-driven PATH line written" \
        grep -Fxq "$DEFAULT_PATH_LINE" "$H14/.profile"

    H15=$TMP/home15
    expect_ok "fish config is detected via SHELL" \
        env HOME="$H15" SHELL=/usr/bin/fish RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "fish_add_path written to config.fish" \
        grep -Fxq 'fish_add_path "$HOME/.local/bin"' "$H15/.config/fish/config.fish"

    H16=$TMP/home16
    expect_ok "--no-add-path never writes a profile" \
        env HOME="$H16" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --no-add-path
    check "no profile written with --no-add-path" test ! -e "$H16/.profile"
    check "--no-add-path still prints PATH guidance" grep -q 'not the active rpi on PATH' "$TMP/last-out"

    # ---- legacy / edge-case PATH behavior --------------------------------

    check "sourced default profile resolves the installed rpi" \
        env HOME="$H10" sh -c '. "$HOME/.profile"; [ "$(command -v rpi)" = "$HOME/.local/bin/rpi" ]'
    check "sourced default profile is idempotent" \
        env HOME="$H10" sh -c '. "$HOME/.profile"; first=$PATH; . "$HOME/.profile"; [ "$first" = "$PATH" ]'

    H17=$TMP/home17
    mkdir -p "$H17"
    printf 'export PATH="%s/.local/bin:$PATH"\n' "$H17" >"$H17/.profile"
    expect_ok "legacy absolute default PATH line is recognized" \
        env HOME="$H17" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "legacy absolute line is not duplicated with a marker" \
        test "$(grep -Fxc '# rpi' "$H17/.profile")" -eq 0

    H18=$TMP/home18
    expect_ok "HOME with a trailing slash is user-scoped" \
        env HOME="$H18/" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "trailing-slash HOME writes the profile" test -f "$H18/.profile"
    check "trailing-slash HOME does not print a doubled profile path" \
        sh -c '! grep -q "//.profile" "$1"' _ "$TMP/last-out"

    H19=$TMP/home19
    SPECIAL_BIN="$H19/bin with space/\$dollar/\"quote\"/\`tick\`"
    expect_ok "special-character prefix installs with --add-path" \
        env HOME="$H19" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$SPECIAL_BIN" --add-path
    check "special-character profile sources in sh" \
        env HOME="$H19" sh -c '. "$HOME/.profile"; [ "${PATH%%:*}" = "$1" ]' _ "$SPECIAL_BIN"
    check "special-character profile keeps a single marker" \
        test "$(grep -Fxc '# rpi' "$H19/.profile")" -eq 1

    H20=$TMP/home20
    mkdir -p "$H20/dotfiles"
    printf 'before\n' >"$H20/dotfiles/zshrc"
    chmod 600 "$H20/dotfiles/zshrc"
    ln -s dotfiles/zshrc "$H20/.zshrc"
    expect_ok "first --add-path through a symlinked profile" \
        env HOME="$H20" SHELL=/bin/zsh RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$H20/binA" --add-path
    expect_ok "second --add-path rewrites the symlinked profile" \
        env HOME="$H20" SHELL=/bin/zsh RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --prefix "$H20/binB" --add-path
    check "symlinked profile stays a symlink" test -L "$H20/.zshrc"
    check "symlinked profile target keeps mode 600" test "$(file_mode "$H20/dotfiles/zshrc")" = 600
    check "symlinked profile target has one block" \
        test "$(grep -Fxc '# rpi' "$H20/dotfiles/zshrc")" -eq 1
    check "symlinked profile target names the new prefix" grep -q "$H20/binB" "$H20/dotfiles/zshrc"

    H21=$TMP/home21
    mkdir -p "$H21"
    printf '# bash profile\n' >"$H21/.bash_profile"
    expect_ok "bash .bash_profile is preferred when .bashrc is absent" \
        env HOME="$H21" SHELL=/bin/bash RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "block written to .bash_profile" \
        grep -Fxq "$DEFAULT_PATH_LINE" "$H21/.bash_profile"
    check "no .profile is created when .bash_profile exists" test ! -e "$H21/.profile"

    H22=$TMP/home22
    expect_ok "unsupported shell falls back to guidance" \
        env HOME="$H22" SHELL=/bin/csh RPI_GITHUB_API_URL="$API_URL" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" RPI_RELEASE_BASE_URL="$BASE/releases" \
        sh "$INSTALL_SH" --add-path
    check "unsupported shell writes no profile" test ! -e "$H22/.profile"
    check "unsupported shell note is printed" \
        grep -q 'not supported for shell csh' "$TMP/last-out"

    H23=$TMP/home23
    expect_ok "RPI_INSTALL_ADD_PATH=True is normalized" \
        env HOME="$H23" SHELL=/bin/bash RPI_INSTALL_ADD_PATH=True \
        RPI_GITHUB_API_URL="$API_URL" RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
        RPI_RELEASE_BASE_URL="$BASE/releases" sh "$INSTALL_SH"
    check "mixed-case env writes the profile" \
        grep -Fxq "$DEFAULT_PATH_LINE" "$H23/.profile"

    H24=$TMP/home24
    expect_fail "--add-path and --no-add-path are rejected" \
        env HOME="$H24" sh "$INSTALL_SH" --add-path --no-add-path
    check "mutual-exclusion message is printed" \
        grep -q 'mutually exclusive' "$TMP/last-out"

    # ---- interactive prompt (pty) ---------------------------------------

    if command -v script >/dev/null 2>&1 &&
        command -v timeout >/dev/null 2>&1 &&
        command -v bash >/dev/null 2>&1 &&
        script --help 2>&1 | grep -q -- '-e'; then
        cat >"$TMP/pty-install-sh.sh" <<EOF
#!/bin/sh
cat "$INSTALL_SH" | sh
EOF
        chmod 755 "$TMP/pty-install-sh.sh"
        cat >"$TMP/pty-install-bash.sh" <<EOF
#!/bin/bash
cat "$INSTALL_SH" | bash
EOF
        chmod 755 "$TMP/pty-install-bash.sh"

        H25=$TMP/home25
        if printf 'n\n' | timeout 60 script -qec "env HOME=$H25 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH= RPI_NONINTERACTIVE= RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases sh $TMP/pty-install-sh.sh" /dev/null >"$TMP/pty-n-out" 2>&1; then
            pass "pty prompt n exits 0"
        else
            fail "pty prompt n exits 0"
            sed 's/^/    | /' "$TMP/pty-n-out"
        fi
        check "pty prompt n leaves the profile untouched" test ! -e "$H25/.profile"
        check "pty prompt n prints guidance" grep -q 'not the active rpi on PATH' "$TMP/pty-n-out"

        H26=$TMP/home26
        if printf 'y\n' | timeout 60 script -qec "env HOME=$H26 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH= RPI_NONINTERACTIVE= RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases sh $TMP/pty-install-sh.sh" /dev/null >"$TMP/pty-y-out" 2>&1; then
            pass "pty prompt y exits 0"
        else
            fail "pty prompt y exits 0"
            sed 's/^/    | /' "$TMP/pty-y-out"
        fi
        check "pty prompt y writes the profile" test -f "$H26/.profile"
        check "pty prompt y writes the block" grep -Fxq "$DEFAULT_PATH_LINE" "$H26/.profile"

        H27=$TMP/home27
        if (sleep 5) | timeout 25 script -qec "env HOME=$H27 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH= RPI_NONINTERACTIVE= RPI_INSTALL_PROMPT_TIMEOUT=1 RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases bash $TMP/pty-install-bash.sh" /dev/null >"$TMP/pty-timeout-out" 2>&1; then
            pass "pty timeout prompt exits 0"
        else
            fail "pty timeout prompt exits 0"
            sed 's/^/    | /' "$TMP/pty-timeout-out"
        fi
        check "pty timeout leaves the profile untouched" test ! -e "$H27/.profile"
        check "pty timeout prints guidance" grep -q 'not the active rpi on PATH' "$TMP/pty-timeout-out"

        # ---- interactive profile prompt + L0 consent ---------------------
        H28=$TMP/home28
        if printf '\n' | timeout 90 script -qec "env HOME=$H28 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH=0 RPI_INSTALL_PROFILE= RPI_NONINTERACTIVE= RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases RPI_SITE_BASE_URL=$BASE sh $TMP/pty-install-sh.sh" /dev/null >"$TMP/pty-profile-default-out" 2>&1; then
            pass "pty Enter accepts the standard default"
        else
            fail "pty Enter accepts the standard default"
            sed 's/^/    | /' "$TMP/pty-profile-default-out"
        fi
        check "profile prompt lists the three profiles" grep -q 'Choose an install profile' "$TMP/pty-profile-default-out"
        check "profile prompt shows archive sizes" grep -q 'MB)' "$TMP/pty-profile-default-out"
        check "Enter installs standard with 6/6 plugins" grep -q '6/6 installed' "$TMP/pty-profile-default-out"
        check "Enter seeds settings" test -f "$H28/.rpi/agent/settings.json"

        H29=$TMP/home29
        if printf '1\n' | timeout 90 script -qec "env HOME=$H29 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH=0 RPI_INSTALL_PROFILE= RPI_NONINTERACTIVE= RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases RPI_SITE_BASE_URL=$BASE sh $TMP/pty-install-sh.sh" /dev/null >"$TMP/pty-profile-mini-out" 2>&1; then
            pass "pty choice 1 installs mini"
        else
            fail "pty choice 1 installs mini"
            sed 's/^/    | /' "$TMP/pty-profile-mini-out"
        fi
        check "pty mini install writes no settings" test ! -e "$H29/.rpi/agent/settings.json"
        check "pty mini output names the mini profile" grep -q 'profile mini' "$TMP/pty-profile-mini-out"

        H30=$TMP/home30-pty
        if (sleep 5) | timeout 30 script -qec "env HOME=$H30 SHELL=/bin/bash CI= TERM=xterm RPI_INSTALL_ADD_PATH=0 RPI_INSTALL_PROFILE= RPI_NONINTERACTIVE= RPI_INSTALL_PROMPT_TIMEOUT=1 RPI_GITHUB_API_URL=$API_URL RPI_VERSION_CHECK_URL=$ENDPOINT_URL RPI_RELEASE_BASE_URL=$BASE/releases RPI_SITE_BASE_URL=$BASE bash $TMP/pty-install-bash.sh" /dev/null >"$TMP/pty-profile-timeout-out" 2>&1; then
            fail "pty profile timeout is refused (unexpected success)"
        else
            pass "pty profile timeout is refused"
        fi
        check "profile timeout is announced" grep -q 'profile prompt timed out' "$TMP/pty-profile-timeout-out"
        check "profile timeout refuses native plugins" grep -q 'refusing to install native plugins' "$TMP/pty-profile-timeout-out"
        check "profile timeout installs nothing" test ! -e "$H30/.local/bin/rpi"
    else
        skip "pty prompt tests (script -e/timeout/bash unavailable)"
    fi
fi

# ---- 11. install profiles, bundle integrity, local materialization ----------------

# Reusable invoker: tests override the release/site/api/agent/cache endpoints
# through RPI_TEST_* without rewriting the whole env for every case.
RPI_TEST_API=""
RPI_TEST_RELEASE=""
RPI_TEST_SITE=""
RPI_TEST_AGENT=""
RPI_TEST_CACHE=""
RPI_TEST_MARKER=""
reset_test_overrides() {
    RPI_TEST_API=""
    RPI_TEST_RELEASE=""
    RPI_TEST_SITE=""
    RPI_TEST_AGENT=""
    RPI_TEST_CACHE=""
    RPI_TEST_MARKER=""
}
run_install() { # run_install <home> <install.sh args...>
    _home=$1
    shift
    env HOME="$_home" \
        RPI_GITHUB_API_URL="${RPI_TEST_API:-$API_URL}" \
        RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
        RPI_RELEASE_BASE_URL="${RPI_TEST_RELEASE:-$BASE/releases}" \
        RPI_SITE_BASE_URL="${RPI_TEST_SITE:-$BASE}" \
        RPI_CODING_AGENT_DIR="${RPI_TEST_AGENT:-}" \
        RPI_INSTALL_CACHE_DIR="${RPI_TEST_CACHE:-}" \
        RPI_TEST_TRUNCATE_MARKER="${RPI_TEST_MARKER:-$TMP/truncate-once}" \
        sh "$INSTALL_SH" "$@"
}

# Request-log helpers: the shared server log lets a case prove what was (and
# was not) requested.
log_mark() { wc -l <"$TMP/server.log" | tr -d ' '; }
log_since() { tail -n +"$(( $1 + 1 ))" "$TMP/server.log"; }

manifest_field() { # manifest_field <manifest> <key>
    sed -n 's/^[[:space:]]*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}

# 11.1 Non-interactive standard/full without --yes must fail before any
# profile-index read or artifact download.
H30=$TMP/home30
before=$(log_mark)
expect_fail "standard without consent fails (no TTY)" run_install "$H30" --profile standard
check "refusal names the consent choices" grep -q 'refusing to install native plugins' "$TMP/last-out"
check "refusal mentions --yes" grep -q -- '--yes' "$TMP/last-out"
check "refusal mentions --profile mini" grep -q -- '--profile mini' "$TMP/last-out"
check "no binary after refusal" test ! -e "$H30/.local/bin/rpi"
check "no settings after refusal" test ! -e "$H30/.rpi/agent/settings.json"
check "no profile-index request before consent" \
    sh -c '! tail -n +$(( $1 + 1 )) "$2" | grep -q "/api/install-profiles/"' _ "$before" "$TMP/server.log"

# 11.2 Trusted standard install: one bundle + sidecar + one index read, six
# plugins materialized locally, settings seeded.
H31=$TMP/home31
before=$(log_mark)
expect_ok "standard --yes installs the bundle" run_install "$H31" --profile standard --yes
check "standard manifest version" \
    test "$(manifest_field "$H31/.local/bin/rpi.install.json" version)" = "$VERSION"
check "standard manifest target" \
    test "$(manifest_field "$H31/.local/bin/rpi.install.json" target)" = "$HOST_TARGET"
check "standard manifest sourceUrl names the bundle" \
    grep -q "rpi-$VERSION-$HOST_TARGET-standard.tar.gz" "$H31/.local/bin/rpi.install.json"
check "standard output reports 6/6 plugins" grep -q '6/6 installed' "$TMP/last-out"
check "standard output reports seeded settings" grep -q 'settings seeded:' "$TMP/last-out"
check "standard installs six plugin directories" \
    test "$(find "$H31/.rpi/agent/extensions" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')" = 6
check "standard settings packages entry lists six plugins" \
    grep -q '"rpi-subagents"' "$H31/.rpi/agent/settings.json"
check "standard settings carry the statusline default" \
    grep -q '"statusLine"' "$H31/.rpi/agent/settings.json"
check "standard settings have no usage plugin" \
    sh -c '! grep -q "rpi-usage" "$1"' _ "$H31/.rpi/agent/settings.json"
delta=$(log_since "$before")
check "exactly one profile-index request" \
    test "$(printf '%s\n' "$delta" | grep -c '/api/install-profiles/')" = 1
check "no plugin artifact was downloaded from the network" \
    sh -c '! printf "%s\n" "$1" | grep -q "\.rpix"' _ "$delta"
check "the bundle and its sidecar were fetched" \
    sh -c 'printf "%s\n" "$1" | grep -q "standard.tar.gz" && printf "%s\n" "$1" | grep -q "standard.tar.gz.sha256"' _ "$delta"
check "installed bundle binary runs" "$H31/.local/bin/rpi"

# 11.3 Full profile adds rpi-usage.
H32=$TMP/home32
expect_ok "full --yes installs the bundle" run_install "$H32" --profile full --yes
check "full output reports 7/7 plugins" grep -q '7/7 installed' "$TMP/last-out"
check "full settings include rpi-usage" grep -q '"rpi-usage"' "$H32/.rpi/agent/settings.json"
check "full installs seven plugin directories" \
    test "$(find "$H32/.rpi/agent/extensions" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')" = 7

# 11.4 Environment-only profile + consent.
H33=$TMP/home33
expect_ok "RPI_INSTALL_PROFILE/RPI_INSTALL_YES installs standard" \
    env HOME="$H33" RPI_INSTALL_PROFILE=standard RPI_INSTALL_YES=1 \
    RPI_GITHUB_API_URL="$API_URL" RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
    RPI_RELEASE_BASE_URL="$BASE/releases" RPI_SITE_BASE_URL="$BASE" \
    sh "$INSTALL_SH"
check "env-driven standard seeded settings" test -f "$H33/.rpi/agent/settings.json"

# 11.5 RPI_CODING_AGENT_DIR is respected.
H34=$TMP/home34
AGENT34=$TMP/agent34
RPI_TEST_AGENT=$AGENT34
expect_ok "custom agent dir installs standard" run_install "$H34" --profile standard --yes
check "plugins land in the custom agent dir" \
    test -d "$AGENT34/extensions/rpi-subagents"
check "settings land in the custom agent dir" test -f "$AGENT34/settings.json"
check "HOME agent dir is untouched" test ! -e "$H34/.rpi/agent/settings.json"
reset_test_overrides

# 11.6 Index failure matrix: missing / version mismatch / missing asset /
# malformed hash all fail closed; never auto-mini.
mkdir -p "$WWW/bad-sites/no-index" "$WWW/bad-sites/version/api/install-profiles" \
    "$WWW/bad-sites/missing/api/install-profiles" "$WWW/bad-sites/hash/api/install-profiles"
printf '{"schemaVersion": 1, "version": "9.9.9", "assets": {}}\n' \
    >"$WWW/bad-sites/version/api/install-profiles/$VERSION.json"
write_profile_index "$WWW/bad-sites/missing/api/install-profiles/$VERSION.json" \
    "$VERSION" "some-other-asset.tar.gz" "$(printf '%064d' 0)"
printf '{\n  "schemaVersion": 1,\n  "version": "%s",\n  "assets": {\n    "rpi-%s-%s-standard.tar.gz": "not-a-hash"\n  }\n}\n' \
    "$VERSION" "$VERSION" "$HOST_TARGET" >"$WWW/bad-sites/hash/api/install-profiles/$VERSION.json"

H35=$TMP/home35
RPI_TEST_SITE=$BASE/bad-sites/no-index
expect_fail "missing profile index fails closed" run_install "$H35" --profile standard --yes
check "missing index explains --profile mini" grep -q -- '--profile mini' "$TMP/last-out"
check "missing index leaves no binary" test ! -e "$H35/.local/bin/rpi"
reset_test_overrides

H36=$TMP/home36
RPI_TEST_SITE=$BASE/bad-sites/version
expect_fail "index version mismatch fails" run_install "$H36" --profile standard --yes
check "version mismatch is reported" grep -q 'version mismatch' "$TMP/last-out"
reset_test_overrides

H37=$TMP/home37
RPI_TEST_SITE=$BASE/bad-sites/missing
expect_fail "missing index asset entry fails" run_install "$H37" --profile standard --yes
check "missing entry is reported" grep -q 'asset entry missing' "$TMP/last-out"
reset_test_overrides

H38=$TMP/home38
RPI_TEST_SITE=$BASE/bad-sites/hash
expect_fail "malformed index hash fails" run_install "$H38" --profile standard --yes
check "malformed hash is reported" grep -q 'not a 64-hex sha256' "$TMP/last-out"
reset_test_overrides

# 11.7 Sidecar/index disagreement is fatal (no fallback to a different source).
mkdir -p "$WWW/bad-sites/mismatch/api/install-profiles"
write_profile_index "$WWW/bad-sites/mismatch/api/install-profiles/$VERSION.json" \
    "$VERSION" "rpi-$VERSION-$HOST_TARGET-standard.tar.gz" "$(printf '%064d' 1)"
H39=$TMP/home39
RPI_TEST_SITE=$BASE/bad-sites/mismatch
expect_fail "sidecar/index mismatch is fatal" run_install "$H39" --profile standard --yes
check "mismatch names the profile index" grep -q 'rpi-pages profile index' "$TMP/last-out"
check "mismatch leaves no binary" test ! -e "$H39/.local/bin/rpi"
reset_test_overrides

# 11.8 Inner plugin failures are isolated: the binary installs, the other
# plugins materialize, the install exits 0 and settings are not seeded.
variant_release() { # variant_release <mode> <profile>
    _mode=$1 _prof=${2:-standard}
    _root=$WWW/variant-$_mode
    make_fake_bundle "$_root" "$VERSION" "$HOST_TARGET" "$_prof" "$_mode"
    _asset=rpi-$VERSION-$HOST_TARGET-$_prof.tar.gz
    _sha=$(sha_of "$_root/releases/download/v$VERSION" "$_asset")
    write_profile_index "$WWW/variant-$_mode-site/api/install-profiles/$VERSION.json" \
        "$VERSION" "$_asset" "$_sha"
    RPI_TEST_API=$DEAD_URL
    RPI_TEST_RELEASE=$BASE/variant-$_mode/releases
    RPI_TEST_SITE=$BASE/variant-$_mode-site
}

H40=$TMP/home40
variant_release bad-plugin-sidecar
expect_ok "one bad plugin sidecar keeps the binary and exits 0" run_install "$H40" --profile standard --yes
check "bad plugin sidecar warns 1 of 6" grep -q '1 of 6 plugins failed' "$TMP/last-out"
check "bad plugin sidecar installs the binary" test -x "$H40/.local/bin/rpi"
check "bad plugin sidecar still installs five plugins" \
    test "$(find "$H40/.rpi/agent/extensions" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')" = 5
check "partial plugin failure skips settings seeding" \
    test ! -e "$H40/.rpi/agent/settings.json"
reset_test_overrides

_plugin_cases='abi2 minhost-new minhost-bad extra badsum missingentry'
# The escape fixture needs Python's tarfile to emit a ../ member safely.
command -v python3 >/dev/null 2>&1 && _plugin_cases="$_plugin_cases escape"
for _case in $_plugin_cases; do
    _h=$TMP/home-plugin-$_case
    variant_release "$_case"
    expect_ok "plugin case $_case is isolated (exit 0)" run_install "$_h" --profile standard --yes
    check "plugin case $_case warns 1 of 6" grep -q '1 of 6 plugins failed' "$TMP/last-out"
    check "plugin case $_case keeps the binary" test -x "$_h/.local/bin/rpi"
    check "plugin case $_case leaves no .tmp/.old residue" \
        test "$(find "$_h/.rpi/agent/extensions" -maxdepth 1 \( -name '*.tmp-*' -o -name '*.old-*' \) | wc -l | tr -d ' ')" = 0
    if [ "$_case" = escape ]; then
        check "escape rpix writes nothing outside the plugin temp dir" \
            test ! -e "$_h/.rpi/agent/extensions/evil"
    fi
    reset_test_overrides
done

H41=$TMP/home41
variant_release minhost-pre
expect_ok "prerelease minHostVersion below the bundle is accepted" run_install "$H41" --profile standard --yes
check "prerelease minHostVersion installs 6/6" grep -q '6/6 installed' "$TMP/last-out"
reset_test_overrides

# 11.9 Unsafe outer archive is fatal before anything is written.
if command -v python3 >/dev/null 2>&1; then
make_unsafe_bundle "$WWW/variant-unsafe-outer" "$VERSION" "$HOST_TARGET" standard
_unsafe_asset=rpi-$VERSION-$HOST_TARGET-standard.tar.gz
_unsafe_sha=$(sha_of "$WWW/variant-unsafe-outer/releases/download/v$VERSION" "$_unsafe_asset")
write_profile_index "$WWW/variant-unsafe-outer-site/api/install-profiles/$VERSION.json" \
    "$VERSION" "$_unsafe_asset" "$_unsafe_sha"
H42=$TMP/home42
RPI_TEST_API=$DEAD_URL
RPI_TEST_RELEASE=$BASE/variant-unsafe-outer/releases
RPI_TEST_SITE=$BASE/variant-unsafe-outer-site
expect_fail "unsafe outer archive is fatal" run_install "$H42" --profile standard --yes
check "unsafe outer archive is reported" grep -q 'unsafe archive member' "$TMP/last-out"
check "unsafe outer archive leaves no binary" test ! -e "$H42/.local/bin/rpi"
reset_test_overrides
else
    skip "unsafe outer archive (python3 unavailable for the tarfile fixture)"
fi

# 11.10 Existing settings are byte-for-byte untouched (and a second run stays
# idempotent).
H43=$TMP/home43
mkdir -p "$H43/.rpi/agent"
printf '{\n  "packages": [],\n  "theme": "custom"\n}\n' >"$H43/.rpi/agent/settings.json"
_settings_before=$(sha_of "$H43/.rpi/agent" settings.json)
expect_ok "existing settings install still materializes plugins" run_install "$H43" --profile standard --yes
check "existing settings are byte-identical" \
    test "$(sha_of "$H43/.rpi/agent" settings.json)" = "$_settings_before"
check "existing settings summary is not misreported as registered" \
    grep -q 'existing settings left untouched:' "$TMP/last-out"
check "existing settings are reported as untouched" grep -q 'left untouched' "$TMP/last-out"
expect_ok "second standard install succeeds" run_install "$H43" --profile standard --yes
check "second install keeps settings byte-identical" \
    test "$(sha_of "$H43/.rpi/agent" settings.json)" = "$_settings_before"

# 11.11 Upgrade failure keeps the previous binary and manifest.
H44=$TMP/home44
expect_ok "baseline mini install" run_install "$H44" --profile mini
_bin_before=$(sha_of "$H44/.local/bin" rpi)
_manifest_before=$(sha_of "$H44/.local/bin" rpi.install.json)
RPI_TEST_SITE=$BASE/bad-sites/mismatch
expect_fail "failed upgrade exits non-zero" run_install "$H44" --profile standard --yes
check "failed upgrade keeps the old binary" \
    test "$(sha_of "$H44/.local/bin" rpi)" = "$_bin_before"
check "failed upgrade keeps the old manifest" \
    test "$(sha_of "$H44/.local/bin" rpi.install.json)" = "$_manifest_before"

# Same-version reinstall messaging, then a real version change.
expect_ok "same-version reinstall succeeds" run_install "$H44" --profile mini
check "same-version reinstall says reinstalling" grep -q 'already installed; reinstalling' "$TMP/last-out"
if sed -i.bak 's/"version": "[^"]*"/"version": "0.0.9"/' "$H44/.local/bin/rpi.install.json" 2>/dev/null; then
    rm -f "$H44/.local/bin/rpi.install.json.bak"
else
    sed 's/"version": "[^"]*"/"version": "0.0.9"/' "$H44/.local/bin/rpi.install.json" >"$TMP/m.tmp"
    mv "$TMP/m.tmp" "$H44/.local/bin/rpi.install.json"
fi
expect_ok "upgrade from an older manifest succeeds" run_install "$H44" --profile mini
check "upgrade prints the current -> target line" grep -q 'Upgrading rpi 0.0.9 -> 0.1.0' "$TMP/last-out"
reset_test_overrides

# 11.12 Static fallback refuses to replace a binary that has existing native
# plugins (only reachable on a glibc host, where --musl means static).
if [ "$host_os" = Linux ] && [ "$MUSL_TARGET" != "$HOST_TARGET" ]; then
    H45=$TMP/home45
    mkdir -p "$H45/.rpi/agent/extensions/existing"
    printf '{\n  "name": "existing",\n  "version": "0.1.0",\n  "native": "libexisting.so",\n  "rpiAbi": 1\n}\n' \
        >"$H45/.rpi/agent/extensions/existing/rpi-extension.json"
    printf 'so\n' >"$H45/.rpi/agent/extensions/existing/libexisting.so"
    expect_fail "static fallback refuses existing native plugins" \
        run_install "$H45" --musl --static-fallback
    check "native precheck lists the plugin directory" grep -q 'existing native plugins' "$TMP/last-out"
    check "native precheck does not delete the plugin" \
        test -f "$H45/.rpi/agent/extensions/existing/libexisting.so"
    check "native precheck leaves no binary" test ! -e "$H45/.local/bin/rpi"

    H45B=$TMP/home45b
    mkdir -p "$H45B/.rpi/agent" "$TMP/settings-native"
    printf 'so\n' >"$TMP/settings-native/libfromsettings.so"
    printf '{\n  "extensions": ["%s"]\n}\n' "$TMP/settings-native/libfromsettings.so" \
        >"$H45B/.rpi/agent/settings.json"
    expect_fail "static fallback refuses native plugin files listed in settings" \
        run_install "$H45B" --musl --static-fallback
    check "settings-listed native file is reported" grep -q 'existing native plugins' "$TMP/last-out"
    check "settings-listed native file is not deleted" \
        test -f "$TMP/settings-native/libfromsettings.so"
else
    skip "static fallback native-plugin precheck (needs a glibc host with --musl)"
fi

# 11.13 --static-fallback is consent, not a force switch: the dynamic build
# wins when it is usable.
if [ "$HOST_TARGET" != "$MUSL_TARGET" ]; then
    H46=$TMP/home46
    expect_ok "static consent is ignored when dynamic is usable" \
        run_install "$H46" --profile mini --static-fallback
    check "dynamic asset installed despite static consent" \
        grep -q "rpi-$VERSION-$HOST_TARGET.tar.gz" "$H46/.local/bin/rpi.install.json"
    check "ignored static consent is explained" grep -q 'fallback is ignored' "$TMP/last-out"
else
    skip "static consent ignored when dynamic is usable (musl host)"
fi

# 11.14 Resume: a partial cache entry with the matching validator resumes with
# a 206; a stale validator forces a clean 200 restart.
HOST_ARCHIVE=rpi-$VERSION-$HOST_TARGET.tar.gz
_full_sha=$(sha_of "$WWW/releases/download/v$VERSION" "$HOST_ARCHIVE")
_full_size=$(wc -c <"$WWW/releases/download/v$VERSION/$HOST_ARCHIVE" | tr -d ' ')
_half=$((_full_size / 2))
_etag=$(printf '"%s"' "$(printf '%s' "$_full_sha" | cut -c1-32)")

seed_partial_cache() { # seed_partial_cache <cachedir> <etag>
    mkdir -p "$1/parts"
    head -c "$_half" "$WWW/releases/download/v$VERSION/$HOST_ARCHIVE" >"$1/parts/$HOST_ARCHIVE"
    {
        printf 'sha=%s\nasset=%s\nversion=%s\netag=%s\nlast_modified=\nurl=%s\n' \
            "$_full_sha" "$HOST_ARCHIVE" "$VERSION" "$2" \
            "$BASE/releases/download/v$VERSION/$HOST_ARCHIVE"
    } >"$1/parts/$HOST_ARCHIVE.meta"
}

CACHE47=$TMP/cache47
seed_partial_cache "$CACHE47" "$_etag"
H47=$TMP/home47
RPI_TEST_CACHE=$CACHE47
before=$(log_mark)
expect_ok "matching-validator resume succeeds" run_install "$H47" --profile mini
delta=$(log_since "$before")
check "resume used a Range request with 206" \
    sh -c 'printf "%s\n" "$1" | grep -q " 206 "' _ "$delta"
check "resume did not restart from zero" \
    sh -c '! grep -q "restarting from zero" "$1"' _ "$TMP/last-out"
check "resumed archive verifies" test -x "$H47/.local/bin/rpi"
reset_test_overrides

CACHE48=$TMP/cache48
seed_partial_cache "$CACHE48" '"stale-validator"'
H48=$TMP/home48
RPI_TEST_CACHE=$CACHE48
expect_ok "stale-validator resume restarts and succeeds" run_install "$H48" --profile mini
check "stale validator warns about the restart" grep -q 'ignored the range request' "$TMP/last-out"
check "stale-validator download still verifies" test -x "$H48/.local/bin/rpi"
reset_test_overrides

# 11.14b A 206 without a response validator must discard the partial and
# restart from zero, even if the cached validator still matches. The fixture
# returns wrong bytes so appending would fail the final sha256 check.
: >"$TMP/no-validator-once"
CACHE48B=$TMP/cache48b
seed_partial_cache "$CACHE48B" "$_etag"
H48B=$TMP/home48b
RPI_TEST_CACHE=$CACHE48B
before=$(log_mark)
expect_ok "206 without a response validator restarts and succeeds" run_install "$H48B" --profile mini
delta=$(log_since "$before")
check "206 without a validator warns about the missing validator" \
    grep -q 'resume validator missing' "$TMP/last-out"
check "206 without a validator restart made a clean 200 request" \
    sh -c 'printf "%s\n" "$1" | grep -q " 200 "' _ "$delta"
check "206 without a validator installed a verified binary" test -x "$H48B/.local/bin/rpi"
check "206 no-validator fixture was consumed" test ! -e "$TMP/no-validator-once"
reset_test_overrides

# A first download interrupted mid-body must keep its bytes and resume with a
# 206 on the next attempt (validators are persisted before success).
: >"$TMP/truncate-once"
CACHE49=$TMP/cache49
H49=$TMP/home49
RPI_TEST_CACHE=$CACHE49
before=$(log_mark)
expect_ok "interrupted first download resumes instead of restarting" run_install "$H49" --profile mini
delta=$(log_since "$before")
check "interrupted download left its partial in the cache" \
    test -s "$CACHE49/parts/$HOST_ARCHIVE"
check "resume after truncation used 206" \
    sh -c 'printf "%s\n" "$1" | grep -q " 206 "' _ "$delta"
check "interrupted download did not restart from zero" \
    sh -c '! grep -q "restarting from zero" "$1"' _ "$TMP/last-out"
check "truncated fixture was consumed" test ! -e "$TMP/truncate-once"
check "resumed interrupted install produced a binary" test -x "$H49/.local/bin/rpi"
reset_test_overrides

# SIGINT while the transfer is still running: the parent progress loop must
# have persisted the validators, so the partial stays resumable.
: >"$TMP/stall-once"
CACHE49B=$TMP/cache49b
H49B=$TMP/home49b
(
    env HOME="$H49B" RPI_GITHUB_API_URL="$API_URL" RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
        RPI_RELEASE_BASE_URL="$BASE/releases" RPI_SITE_BASE_URL="$BASE" \
        RPI_INSTALL_CACHE_DIR="$CACHE49B" RPI_INSTALL_ADD_PATH=0 \
        sh "$INSTALL_SH" --profile mini >"$TMP/stall-out" 2>&1
) &
_installer_pid=$!
sleep 6
echo "DEBUG before-kill marker=$(test -e "$TMP/stall-once" && echo yes || echo no) part=$(wc -c <"$CACHE49B/parts/$HOST_ARCHIVE" 2>/dev/null || echo 0) meta=$(test -e "$CACHE49B/parts/$HOST_ARCHIVE.meta" && echo yes || echo no)"
ls -la "$CACHE49B/parts" | sed 's/^/  DBG /'
grep STALL "$TMP/server.log" | tail -3 | sed 's/^/  DBG /'
ps -ef | grep -E "curl|install.sh" | grep -v grep | head -8 | sed 's/^/  DBG /'
ps aux | grep -E "curl.*tar.gz|install.sh" | grep -v grep | head -5 | sed 's/^/  DBG /'
cat "$TMP/stall-out" | tail -5 | sed 's/^/  DBG /'
kill -INT "$_installer_pid" 2>/dev/null || true
sleep 1
kill -KILL "$_installer_pid" 2>/dev/null || true
wait "$_installer_pid" 2>/dev/null || true
check "interrupt during transfer leaves a partial" \
    test -s "$CACHE49B/parts/$HOST_ARCHIVE"
check "interrupt during transfer persisted validators" \
    grep -q -E '^(etag|last_modified)=.' "$CACHE49B/parts/$HOST_ARCHIVE.meta"
check "interrupt during transfer did not install a binary" test ! -e "$H49B/.local/bin/rpi"

# A verified cached archive is reused without a network transfer.
H50=$TMP/home50
RPI_TEST_CACHE=$CACHE49
before=$(log_mark)
expect_ok "second install reuses the cached archive" run_install "$H50" --profile mini
delta=$(log_since "$before")
check "cache hit is announced" grep -q "using cached $HOST_ARCHIVE" "$TMP/last-out"
check "cache hit fetched no archive bytes" \
    sh -c '! printf "%s\n" "$1" | grep -q "GET .*rpi-.*\.tar\.gz HTTP"' _ "$delta"
reset_test_overrides

# A tampered cached archive (same meta hash) is re-downloaded, not installed.
printf 'tampered' >>"$CACHE49/parts/$HOST_ARCHIVE"
H50B=$TMP/home50b
RPI_TEST_CACHE=$CACHE49
before=$(log_mark)
expect_ok "tampered cached archive is re-downloaded" run_install "$H50B" --profile mini
delta=$(log_since "$before")
check "tampered cache was replaced by a transfer" \
    sh -c 'printf "%s\n" "$1" | grep -q "GET .*rpi-.*\.tar\.gz HTTP"' _ "$delta"
check "tampered-cache install verified" test -x "$H50B/.local/bin/rpi"
reset_test_overrides

# Non-TTY output must stay clean: no ANSI escapes and no carriage returns.
check "non-TTY install output has no ANSI escape" \
    sh -c '! grep -q "$(printf "\033")" "$1"' _ "$TMP/last-out"
check "non-TTY install output has no carriage return" \
    sh -c '! grep -q "$(printf "\r")" "$1"' _ "$TMP/last-out"

# A lock untouched for over an hour is reclaimed (an active transfer refreshes
# its mtime every 30s, so active locks are never reclaimed by age alone).
CACHE51=$TMP/cache51
mkdir -p "$CACHE51/parts"
: >"$CACHE51/parts/$HOST_ARCHIVE.lock"
touch -t 202001010000 "$CACHE51/parts/$HOST_ARCHIVE.lock"
H51=$TMP/home51
RPI_TEST_CACHE=$CACHE51
expect_ok "stale download lock is reclaimed" run_install "$H51" --profile mini
check "stale lock reclaim is reported" grep -q 'reclaiming a stale download lock' "$TMP/last-out"
check "stale lock install succeeded" test -x "$H51/.local/bin/rpi"
reset_test_overrides

# --no-cache must not populate the configured cache directory.
CACHE52=$TMP/cache52
H52=$TMP/home52
RPI_TEST_CACHE=$CACHE52
expect_ok "--no-cache install succeeds" run_install "$H52" --profile mini --no-cache
check "--no-cache leaves the configured cache empty" \
    test ! -e "$CACHE52/parts/$HOST_ARCHIVE"
reset_test_overrides

# RPI_INSTALL_REQUIRE_PLUGINS turns isolated plugin failures into exit 1.
H53=$TMP/home53
variant_release bad-plugin-sidecar
expect_fail "RPI_INSTALL_REQUIRE_PLUGINS=1 exits non-zero on a plugin failure" \
    env HOME="$H53" RPI_INSTALL_REQUIRE_PLUGINS=1 \
    RPI_GITHUB_API_URL="$DEAD_URL" RPI_VERSION_CHECK_URL="$ENDPOINT_URL" \
    RPI_RELEASE_BASE_URL="$BASE/variant-bad-plugin-sidecar/releases" \
    RPI_SITE_BASE_URL="$BASE/variant-bad-plugin-sidecar-site" \
    sh "$INSTALL_SH" --profile standard --yes
check "require-plugins still warned about the failure" grep -q '1 of 6 plugins failed' "$TMP/last-out"
check "require-plugins kept the binary" test -x "$H53/.local/bin/rpi"
reset_test_overrides

# 11.15 Semver comparison vectors (sourced helper mode; no install).
if ( set --
    RPI_INSTALL_TEST_SOURCE=1 . "$INSTALL_SH"
    pretty='{
  "tag_name": "v0.1.0",
  "assets": [
    {
      "name": "rpi-0.1.0-x86_64-unknown-linux-gnu-standard.tar.gz",
      "size": 33000000,
      "uploader": { "login": "bot" }
    },
    {
      "name": "rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz",
      "size": 19000000
    }
  ]
}'
    compact='{"tag_name":"v0.1.0","assets":[{"name":"rpi-0.1.0-x86_64-unknown-linux-gnu-full.tar.gz","size":34000000}]}'
    compact2='{"assets":[{"name":"rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz","size":19000000},{"name":"rpi-0.1.0-x86_64-unknown-linux-gnu-standard.tar.gz","size":33000000}]}'
    size_ok=1
    [ "$(json_asset_size "$pretty" rpi-0.1.0-x86_64-unknown-linux-gnu-standard.tar.gz)" = 33000000 ] || size_ok=0
    [ "$(json_asset_size "$pretty" rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz)" = 19000000 ] || size_ok=0
    [ "$(json_asset_size "$compact" rpi-0.1.0-x86_64-unknown-linux-gnu-full.tar.gz)" = 34000000 ] || size_ok=0
    [ "$(json_asset_size "$compact2" rpi-0.1.0-x86_64-unknown-linux-gnu-standard.tar.gz)" = 33000000 ] || size_ok=0
    [ "$(json_asset_size "$compact2" rpi-0.1.0-x86_64-unknown-linux-gnu.tar.gz)" = 19000000 ] || size_ok=0
    expect_semver() {
        a=$1 b=$2 want=$3
        _rc=0
        semver_le "$a" "$b" || _rc=$?
        case $_rc in
            0) got=le ;;
            1) got=gt ;;
            *) got=invalid ;;
        esac
        if [ "$got" != "$want" ]; then
            printf 'semver %s vs %s: got %s want %s\n' "$a" "$b" "$got" "$want" >&2
            return 1
        fi
    }
    expect_semver 0.1.0 0.1.0 le &&
    expect_semver 0.1.0-rc.1 0.1.0 le &&
    expect_semver 0.1.0 0.1.0-rc.1 gt &&
    expect_semver 0.1.0-rc.1 0.1.0-rc.2 le &&
    expect_semver 0.1.0-rc.2 0.1.0-rc.10 le &&
    expect_semver 0.1.0-alpha 0.1.0-beta le &&
    expect_semver 0.1.0-alpha 0.1.0-alpha.1 le &&
    expect_semver 0.1.0-alpha.1 0.1.0-alpha gt &&
    expect_semver 0.1.0+build.1 0.1.0 le &&
    expect_semver 1.0.0 0.9.9 gt &&
    expect_semver 0.1.8-rc.3 0.1.8 le &&
    expect_semver 0.1.8 0.1.8-rc.3 gt &&
    expect_semver bad 0.1.0 invalid &&
    expect_semver 1.0 0.1.0 invalid &&
    expect_semver 01.0.0 0.1.0 invalid &&
    expect_semver 0.1.0-01 0.1.0 invalid &&
    [ "$size_ok" = 1 ]
); then
    pass "semver comparison vectors and asset size parsing"
else
    fail "semver comparison vectors and asset size parsing"
fi

# ---- 12. transactional binary/manifest replacement --------------------------------

# Source the helper definitions without running main, then exercise
# install_binary with a fault-injected `mv` that fails only for the manifest
# rename. This is the same failure shape that used to leave a new binary next
# to an old manifest.
if ( set --
    RPI_INSTALL_TEST_SOURCE=1 . "$INSTALL_SH"
    set +e
    TR=$(mktemp -d "${TMPDIR:-/tmp}/rpi-commit.XXXXXX")
    PREFIX=$TR/prefix
    SRC=$TR/src
    FAKEBIN=$TR/bin
    mkdir -p "$PREFIX" "$SRC" "$FAKEBIN"
    printf 'NEW-BINARY\n' >"$SRC/rpi"
    cat >"$FAKEBIN/mv" <<'EOM'
#!/bin/sh
if [ "${FAIL_MANIFEST_MV:-0}" = 1 ]; then
    _src=''
    _dst=''
    for _arg in "$@"; do
        case $_arg in
            *.stage-*) _src=$_arg ;;
            */rpi.install.json) _dst=$_arg ;;
        esac
    done
    [ -z "$_src" ] || [ -z "$_dst" ] || {
        echo 'injected manifest rename failure' >&2
        exit 1
    }
fi
exec /bin/mv "$@"
EOM
    chmod 755 "$FAKEBIN/mv"
    VERSION=0.1.8-rc.4
    TARGET=x86_64-unknown-linux-gnu
    IS_STATIC=0
    ARCHIVE_URL=https://example.invalid/bundle.tar.gz
    EXPECTED_SHA=deadbeef
    STAGE_BIN=""
    STAGE_MANIFEST=""
    printf 'OLD-BINARY\n' >"$PREFIX/rpi"
    printf '{"version":"0.1.8-rc.3"}\n' >"$PREFIX/rpi.install.json"
    old_bin_sha=$(sha256_file "$PREFIX/rpi")
    old_manifest_sha=$(sha256_file "$PREFIX/rpi.install.json")
    ( PATH="$FAKEBIN:$PATH" FAIL_MANIFEST_MV=1 install_binary "$SRC" rpi ) >/dev/null 2>&1
    upgrade_rc=$?
    upgrade_ok=1
    [ "$upgrade_rc" -ne 0 ] || upgrade_ok=0
    [ "$(sha256_file "$PREFIX/rpi")" = "$old_bin_sha" ] || upgrade_ok=0
    [ "$(sha256_file "$PREFIX/rpi.install.json")" = "$old_manifest_sha" ] || upgrade_ok=0
    [ -z "$(find "$PREFIX" -maxdepth 1 \( -name '*.stage-*' -o -name '*.backup-*' \) -print -quit)" ] || upgrade_ok=0

    mkdir -p "$TR/first"
    first_prefix=$TR/first
    ( PATH="$FAKEBIN:$PATH" FAIL_MANIFEST_MV=1 PREFIX=$first_prefix install_binary "$SRC" rpi ) >/dev/null 2>&1
    first_rc=$?
    first_ok=1
    [ "$first_rc" -ne 0 ] || first_ok=0
    [ ! -e "$first_prefix/rpi" ] || first_ok=0
    [ ! -e "$first_prefix/rpi.install.json" ] || first_ok=0
    [ -z "$(find "$first_prefix" -maxdepth 1 \( -name '*.stage-*' -o -name '*.backup-*' \) -print -quit)" ] || first_ok=0

    ( PATH="$FAKEBIN:$PATH" FAIL_MANIFEST_MV=0 install_binary "$SRC" rpi ) >/dev/null 2>&1
    success_rc=$?
    success_ok=1
    [ "$success_rc" -eq 0 ] || success_ok=0
    [ "$(cat "$PREFIX/rpi")" = 'NEW-BINARY' ] || success_ok=0
    grep -q '"version": "0.1.8-rc.4"' "$PREFIX/rpi.install.json" || success_ok=0

    rm -rf "$TR"
    [ "$upgrade_ok" = 1 ] && [ "$first_ok" = 1 ] && [ "$success_ok" = 1 ]
); then
    pass "transactional replacement preserves the old pair on manifest failure"
else
    fail "transactional replacement preserves the old pair on manifest failure"
fi

# ---- 13. strict profile-index parser ------------------------------------------------

# Exercise read_profile_index directly (source mode) with malformed fixtures.
# The installer's normal profile tests cover the happy path; these cases prove
# duplicate keys, JSON type coercion, nested/flat schema violations and trailing
# data all fail before any artifact is downloaded.
if ( set --
    RPI_INSTALL_TEST_SOURCE=1 . "$INSTALL_SH"
    set +e
    TD=$(mktemp -d "${TMPDIR:-/tmp}/rpi-index.XXXXXX")
    TMPDIR_RPI=$TD
    SITE_BASE_URL=http://example.invalid
    INDEX_SHA=""
    A=rpi-0.1.8-rc.4-x86_64-unknown-linux-musl-standard.tar.gz
    V=0.1.8-rc.4
    S=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
    FIXTURE=""
    fetch_meta() { cp "$FIXTURE" "$2"; }
    parse_case() { # parse_case <file> -> stdout sha, rc
        FIXTURE=$1
        ( read_profile_index "$V" "$A" && printf '%s' "$INDEX_SHA" ) 2>/dev/null
    }
    good='{"assets":{"'"$A"'":"'"$S"'"},"version":"'"$V"'","schemaVersion":1}'
    printf '%s\n' "$good" >"$TD/good.json"
    sha=$(parse_case "$TD/good.json") || { rm -rf "$TD"; exit 1; }
    [ "$sha" = "$S" ] || { rm -rf "$TD"; exit 1; }

    dup_schema='{ "schemaVersion": 1, "schemaVersion": 1, "version": "'"$V"'", "assets": { "'"$A"'": "'"$S"'" } }'
    str_schema='{ "schemaVersion": "1", "version": "'"$V"'", "assets": { "'"$A"'": "'"$S"'" } }'
    outside='{ "schemaVersion": 1, "version": "'"$V"'", "assets": {}, "extra": { "'"$A"'": "'"$S"'" } }'
    nested='{ "schemaVersion": 1, "version": "'"$V"'", "assets": { "nested": { "'"$A"'": "'"$S"'" } } }'
    trailing='{ "schemaVersion": 1, "version": "'"$V"'", "assets": { "'"$A"'": "'"$S"'" } } trailing'
    dup_asset='{ "schemaVersion": 1, "version": "'"$V"'", "assets": { "'"$A"'": "'"$S"'", "'"$A"'": "'"$S"'" } }'
    float_size='{ "schemaVersion": 1, "version": "'"$V"'", "assets": { "'"$A"'": "'"$S"'" }, "sizes": { "'"$A"'": 1.5 } }'
    ok=1
    for name in dup_schema str_schema outside nested trailing dup_asset float_size; do
        eval "content=\$$name"
        printf '%s\n' "$content" >"$TD/$name.json"
        if parse_case "$TD/$name.json"; then ok=0; fi
    done
    head -c 262145 /dev/zero | tr '\000' ' ' >"$TD/large.json"
    if parse_case "$TD/large.json"; then ok=0; fi
    rm -rf "$TD"
    [ "$ok" = 1 ]
); then
    pass "strict profile-index parser rejects duplicate/coerced/nested/trailing input"
else
    fail "strict profile-index parser rejects duplicate/coerced/nested/trailing input"
fi

# ---- 14. cache GC lock mapping + retry backoff jitter -------------------------------

# The GC must map <asset>.meta back to the active <asset>.lock; otherwise an
# in-flight resume loses its validators. The jitter checks pin the 1/2/4/8s
# schedule to +/-20% and prove distinct processes do not share one factor.
if ( set --
    RPI_INSTALL_TEST_SOURCE=1 . "$INSTALL_SH"
    set +e
    TD=$(mktemp -d "${TMPDIR:-/tmp}/rpi-gc.XXXXXX")
    ok=1

    CACHE_PARTS=$TD
    NO_CACHE_FLAG=0
    LOCKED=locked-asset
    printf 'part' >"$TD/$LOCKED"
    printf 'sha=x\n' >"$TD/$LOCKED.meta"
    : >"$TD/$LOCKED.lock"
    printf 'part' >"$TD/stale-asset"
    printf 'sha=y\n' >"$TD/stale-asset.meta"
    touch -t 202001010000 "$TD/$LOCKED" "$TD/$LOCKED.meta" "$TD/$LOCKED.lock" \
        "$TD/stale-asset" "$TD/stale-asset.meta"
    gc_cache
    [ -f "$TD/$LOCKED" ] || ok=0
    [ -f "$TD/$LOCKED.meta" ] || ok=0
    [ ! -e "$TD/stale-asset" ] || ok=0
    [ ! -e "$TD/stale-asset.meta" ] || ok=0

    for base in 1 2 4 8; do
        for _ in 1 2 3; do
            value=$(backoff_seconds "$base")
            awk -v v="$value" -v b="$base" \
                'BEGIN { exit !(v + 0 >= b * 0.8 - 0.005 && v + 0 <= b * 1.2 + 0.005) }' || ok=0
        done
    done
    distinct=$(for _ in 1 2 3 4 5 6; do
        sh -c 'p=$1; set --; RPI_INSTALL_TEST_SOURCE=1 . "$p"; backoff_seconds 8; echo' _ "$INSTALL_SH"
    done | sort -u | wc -l | tr -d ' ')
    [ "$distinct" -ge 2 ] || ok=0

    SLEEP_LOG=$TD/sleep.log
    sleep() { printf '%s\n' "$1" >>"$SLEEP_LOG"; }
    SLEEP_FRACTIONAL=1
    sleep_seconds 0.83
    [ "$(cat "$SLEEP_LOG")" = "0.83" ] || ok=0

    rm -rf "$TD"
    [ "$ok" = 1 ]
); then
    pass "cache GC honours the base asset lock and retry delays carry real jitter"
else
    fail "cache GC honours the base asset lock and retry delays carry real jitter"
fi

# Ordinary 5xx retries and an unparseable Retry-After must sleep a jittered
# fractional delay (the old code slept a flat 1/2/4/8s with no jitter).
for mode in 500 429bad; do
    case $mode in
        500) MARK=$TMP/fail-500-once ;;
        *) MARK=$TMP/fail-429-once ;;
    esac
    : >"$MARK"
    H51=$TMP/home51-$mode
    RPI_TEST_CACHE=$TMP/cache51-$mode
    expect_ok "$mode retry with jittered backoff succeeds" run_install "$H51" --profile mini
    check "$mode retry logged a fractional jittered delay" \
        grep -Eq 'retrying in [0-9]+\.[0-9]{2}s' "$TMP/last-out"
    check "$mode retry fixture was consumed" test ! -e "$MARK"
    reset_test_overrides
done

# ---- summary ------------------------------------------------------------------------


echo
echo "ran $TESTS_RUN checks, $TESTS_FAILED failed"
[ "$TESTS_FAILED" -eq 0 ]
