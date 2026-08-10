#!/bin/sh
# install.sh — official installer for the rpi CLI (POSIX sh; no bash required).
#
# Usage:
#   curl -fsSL https://revpi.dev/install.sh | sh
#   sh install.sh --prefix /usr/local/bin
#   sh install.sh --musl
#
# Options:
#   --prefix <dir>   Install directory (default: ~/.local/bin)
#   --musl           Force the musl target on Linux (auto-detected via ldd otherwise)
#   -h, --help       Show usage
#
# Environment overrides (for mirrors and tests):
#   RPI_GITHUB_API_URL     GitHub API URL for the latest release
#                          (default: https://api.github.com/repos/revpidev/rpi/releases/latest)
#   RPI_VERSION_CHECK_URL  Fallback version endpoint, same contract as the CLI's
#                          (default: https://revpi.dev/api/latest-version)
#   RPI_RELEASE_BASE_URL   Base used to construct GitHub download URLs
#                          (default: https://github.com/revpidev/rpi/releases)
#   RPI_SITE_BASE_URL      Official-site mirror base (default: https://revpi.dev);
#                          assets are fetched from <site>/releases/download/...
#
# Download fallback order (ADR-0011 revision, for networks where GitHub is
# unreachable): 1) the browser_download_url pair returned by the GitHub API
# (API resolution path only), 2) the URL constructed by naming rule from
# RPI_RELEASE_BASE_URL, 3) the official-site mirror under RPI_SITE_BASE_URL.
# A failed DOWNLOAD moves on to the next candidate; a failed sha256
# integrity check is always fatal — an integrity failure never silently
# switches sources.
#
# Security note: the downloaded archive is checked against the .sha256 sidecar
# published next to it. This is an integrity check only (it catches corrupted
# downloads / mirror mix-ups); it does NOT protect against tampered release
# assets. Artifact signing is a planned follow-up (see ADR-0011).

set -eu

REPO="revpidev/rpi"
GITHUB_API_URL=${RPI_GITHUB_API_URL:-"https://api.github.com/repos/$REPO/releases/latest"}
VERSION_CHECK_URL=${RPI_VERSION_CHECK_URL:-"https://revpi.dev/api/latest-version"}
RELEASE_BASE_URL=${RPI_RELEASE_BASE_URL:-"https://github.com/$REPO/releases"}
SITE_BASE_URL=${RPI_SITE_BASE_URL:-"https://revpi.dev"}

PREFIX=""
FORCE_MUSL=0
TMPDIR_RPI=""

info() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Usage: install.sh [--prefix <dir>] [--musl]

Options:
  --prefix <dir>   Install directory (default: ~/.local/bin)
  --musl           Force the musl target on Linux (auto-detected otherwise)
  -h, --help       Show this help

Environment overrides (mirrors/tests): RPI_GITHUB_API_URL,
RPI_VERSION_CHECK_URL, RPI_RELEASE_BASE_URL, RPI_SITE_BASE_URL — see the
script header.
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --prefix)
            [ $# -ge 2 ] || die "--prefix requires a directory argument"
            PREFIX=$2
            shift 2
            ;;
        --prefix=*)
            PREFIX=${1#--prefix=}
            shift
            ;;
        --musl)
            FORCE_MUSL=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (see --help)"
            ;;
    esac
done

if [ -z "$PREFIX" ]; then
    [ -n "${HOME:-}" ] || die "\$HOME is not set; pass --prefix explicitly"
    PREFIX=$HOME/.local/bin
fi

cleanup() {
    [ -z "$TMPDIR_RPI" ] || rm -rf "$TMPDIR_RPI"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# ---- download tool ----------------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    DL_TOOL=curl
elif command -v wget >/dev/null 2>&1; then
    DL_TOOL=wget
else
    die "neither curl nor wget found; install one of them and re-run"
fi

fetch() { # fetch <url> <output-file>
    if [ "$DL_TOOL" = curl ]; then
        # --connect-timeout: fail fast on blocked hosts so the next
        # download candidate is tried quickly.
        curl -fsSL --connect-timeout 10 --retry 3 -o "$2" "$1"
    else
        wget -q --timeout=10 -O "$2" "$1"
    fi
}

fetch_stdout() { # fetch_stdout <url>
    if [ "$DL_TOOL" = curl ]; then
        curl -fsSL --connect-timeout 10 --retry 3 "$1"
    else
        wget -q --timeout=10 -O - "$1"
    fi
}

# ---- platform detection -----------------------------------------------------

TARGET=""
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case $arch in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "unsupported CPU architecture: $arch" ;;
    esac
    case $os in
        Darwin)
            [ "$arch" = aarch64 ] ||
                die "unsupported macOS architecture: $arch (only Apple silicon builds are published)"
            TARGET=aarch64-apple-darwin
            ;;
        Linux)
            if [ "$FORCE_MUSL" = 1 ] ||
                { command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; }; then
                TARGET=$arch-unknown-linux-musl
            else
                TARGET=$arch-unknown-linux-gnu
            fi
            ;;
        *)
            die "unsupported operating system: $os (on Windows, use install.ps1)"
            ;;
    esac
}

# ---- version / asset URL resolution -----------------------------------------

# Print every string value of JSON key $1, one per line. Deliberately
# dependency-free (no jq); release JSON never carries commas inside URLs.
json_values() {
    tr ',' '\n' | sed -n 's/^[^"]*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

VERSION=""
ARCHIVE_URL=""
CHECKSUM_URL=""

# Preferred path: GitHub API — tag_name gives the version, assets give the
# exact browser_download_url (no name construction). Returns non-zero when the
# API is unreachable or the expected assets are missing.
resolve_via_api() {
    body=$(fetch_stdout "$GITHUB_API_URL" 2>/dev/null) || return 1
    [ -n "$body" ] || return 1
    tag=$(printf '%s' "$body" | json_values tag_name | head -n 1)
    [ -n "$tag" ] || return 1
    ver=${tag#v}
    name=rpi-$ver-$TARGET.tar.gz
    archive=""
    checksum=""
    for url in $(printf '%s' "$body" | json_values browser_download_url); do
        case $url in
            */"$name") archive=$url ;;
            */"$name.sha256") checksum=$url ;;
        esac
    done
    [ -n "$archive" ] || return 1
    [ -n "$checksum" ] || return 1
    VERSION=$ver
    ARCHIVE_URL=$archive
    CHECKSUM_URL=$checksum
    return 0
}

# Fallback path: version endpoint ({"version": ...}) + URL construction by
# naming rule. Fatal when the endpoint itself fails; the download stage
# below still falls back to the site mirror.
resolve_via_endpoint() {
    body=$(fetch_stdout "$VERSION_CHECK_URL" 2>/dev/null) ||
        die "version endpoint $VERSION_CHECK_URL is unreachable"
    ver=$(printf '%s' "$body" | json_values version | head -n 1)
    [ -n "$ver" ] || die "version endpoint $VERSION_CHECK_URL did not return a version"
    ver=${ver#v}
    VERSION=$ver
    ARCHIVE_URL=$RELEASE_BASE_URL/download/v$ver/rpi-$ver-$TARGET.tar.gz
    CHECKSUM_URL=$ARCHIVE_URL.sha256
}

detect_target
info "detected platform: $TARGET"

if resolve_via_api; then
    info "latest release: v$VERSION (via GitHub API)"
else
    warn "GitHub API unavailable; falling back to version endpoint"
    resolve_via_endpoint
    info "latest release: v$VERSION (via version endpoint)"
fi

ARCHIVE_NAME=rpi-$VERSION-$TARGET.tar.gz

# ---- download + integrity check ---------------------------------------------

# Ordered download candidates (see the header for the fallback order). The
# API path's browser_download_url comes first; the naming-rule GitHub URL
# and the site mirror follow, deduplicated.
CANDIDATES=""
add_candidate() { # add_candidate <archive-url>
    [ -n "$1" ] || return 0
    case "
$CANDIDATES" in
        *"
$1
"*) return 0 ;;
    esac
    CANDIDATES="$CANDIDATES$1
"
}

add_candidate "$ARCHIVE_URL"
add_candidate "$RELEASE_BASE_URL/download/v$VERSION/$ARCHIVE_NAME"
add_candidate "$SITE_BASE_URL/releases/download/v$VERSION/$ARCHIVE_NAME"

# Reject HTML bodies (e.g. a login page served behind a redirect while the
# repo is private): archive and sidecar must be binary/plain text.
is_html() { head -c 256 "$1" 2>/dev/null | grep -qi '<html'; }

TMPDIR_RPI=$(mktemp -d 2>/dev/null || mktemp -d -t rpi-install)

DOWNLOAD_SOURCE=""
ifs_save=$IFS
IFS='
'
for candidate in $CANDIDATES; do
    IFS=$ifs_save
    info "downloading $candidate"
    if ! fetch "$candidate" "$TMPDIR_RPI/$ARCHIVE_NAME"; then
        warn "download failed from $candidate; trying the next mirror"
        continue
    fi
    if is_html "$TMPDIR_RPI/$ARCHIVE_NAME"; then
        warn "unexpected HTML content from $candidate; trying the next mirror"
        continue
    fi
    # The sidecar URL is <archive>.sha256 by construction — on GitHub and
    # on the site mirror alike.
    if ! fetch "$candidate.sha256" "$TMPDIR_RPI/$ARCHIVE_NAME.sha256"; then
        warn "checksum download failed from $candidate.sha256; trying the next mirror"
        continue
    fi
    if is_html "$TMPDIR_RPI/$ARCHIVE_NAME.sha256"; then
        warn "unexpected HTML checksum from $candidate.sha256; trying the next mirror"
        continue
    fi
    # An integrity failure is fatal — never switch mirrors on a bad
    # checksum. Compare the hash token, not `sha256sum -c`: the check must
    # not depend on the filename embedded in the sidecar (published sidecars
    # used to carry a dist/ path prefix). GNU sha256sum prints
    # "<hash>  <file>", BSD shasum prints "SHA256 (<file>) = <hash>";
    # extract the hex token from each format.
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$TMPDIR_RPI/$ARCHIVE_NAME" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$TMPDIR_RPI/$ARCHIVE_NAME" | awk '{print $NF}')
    else
        die "neither sha256sum nor shasum found; cannot verify download integrity"
    fi
    expected=$(sed 's/[[:space:]].*//' "$TMPDIR_RPI/$ARCHIVE_NAME.sha256" | head -n 1)
    case $expected in
        '' | *[!0-9a-fA-F]*) die "malformed checksum in $ARCHIVE_NAME.sha256" ;;
    esac
    if [ "$actual" != "$expected" ]; then
        warn "expected $expected, got $actual"
        die "sha256 integrity check failed for $ARCHIVE_NAME; aborting"
    fi
    DOWNLOAD_SOURCE=$candidate
    break
done
IFS=$ifs_save

if [ -z "$DOWNLOAD_SOURCE" ]; then
    tried=$(printf '%s' "$CANDIDATES" | tr '\n' ' ')
    die "all download candidates failed for $ARCHIVE_NAME (tried: $tried)"
fi
ARCHIVE_URL=$DOWNLOAD_SOURCE
info "sha256 integrity check passed"

# ---- install ----------------------------------------------------------------

mkdir "$TMPDIR_RPI/x"
tar -xzf "$TMPDIR_RPI/$ARCHIVE_NAME" -C "$TMPDIR_RPI/x" ||
    die "failed to extract $ARCHIVE_NAME"
[ -f "$TMPDIR_RPI/x/rpi" ] || die "archive did not contain an rpi binary"

sudo_hint() {
    die "install directory $PREFIX is not writable — re-run with sudo, e.g.:
  curl -fsSL https://revpi.dev/install.sh | sudo sh -s -- --prefix \"$PREFIX\""
}

if [ ! -d "$PREFIX" ]; then
    mkdir -p "$PREFIX" 2>/dev/null || sudo_hint
fi
[ -w "$PREFIX" ] || sudo_hint

cp "$TMPDIR_RPI/x/rpi" "$PREFIX/rpi" || sudo_hint
chmod 755 "$PREFIX/rpi" || die "failed to chmod 755 $PREFIX/rpi"

PREFIX_ABS=$(cd "$PREFIX" && pwd)
INSTALL_PATH=$PREFIX_ABS/rpi
ARCHIVE_SHA256=$(sed 's/[[:space:]].*//' "$TMPDIR_RPI/$ARCHIVE_NAME.sha256" | head -n 1)
INSTALLED_AT=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

MANIFEST=$PREFIX_ABS/rpi.install.json
cat >"$MANIFEST" <<EOF
{
  "version": "$(json_escape "$VERSION")",
  "target": "$(json_escape "$TARGET")",
  "installedAt": "$(json_escape "$INSTALLED_AT")",
  "sourceUrl": "$(json_escape "$ARCHIVE_URL")",
  "sha256": "$(json_escape "$ARCHIVE_SHA256")",
  "installPath": "$(json_escape "$INSTALL_PATH")",
  "method": "binary"
}
EOF

# ---- guidance ---------------------------------------------------------------

info ""
info "rpi $VERSION installed to $INSTALL_PATH"
info "  target:   $TARGET"
info "  manifest: $MANIFEST"
info ""
info "next steps:"
info "  update:    rpi update --self"
info "  uninstall: rpi self-uninstall   (add --purge to also delete ~/.rpi)"

case :$PATH: in
    *":$PREFIX_ABS:"*) ;;
    *)
        info ""
        info "note: $PREFIX_ABS is not in your PATH; add it with:"
        info "  export PATH=\"$PREFIX_ABS:\$PATH\""
        info "(append that line to ~/.profile, ~/.bashrc or ~/.zshrc to make it permanent)"
        ;;
esac
