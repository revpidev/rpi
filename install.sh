#!/bin/sh
# install.sh — official installer for the rpi CLI (POSIX sh; no bash required).
#
# Usage:
#   curl -fsSL https://revpi.dev/install.sh | sh
#   sh install.sh --profile mini
#   sh install.sh --profile standard --yes
#   sh install.sh --prefix /usr/local/bin
#   sh install.sh --add-path
#   sh install.sh --musl
#
# Profiles (V18-13): the profile only selects which prebuilt archive to fetch
# and install; the rpi binary itself has no profile concept.
#   mini      rpi binary only
#   standard  rpi + six official plugins in ONE pinned archive (default)
#   full      standard + rpi-usage
#
# Options:
#   --prefix <dir>       Install directory (default: ~/.local/bin)
#   --profile <name>     mini|standard|full (default: standard). Naming a
#                        profile does NOT by itself consent to installing
#                        native plugins; see --yes.
#   --yes                Consent to installing the bundle's official native
#                        (L0) plugins without an interactive prompt. Also
#                        RPI_INSTALL_YES=1.
#   --static-fallback    Consent to the static musl fallback if (and only if)
#                        the dynamic runtime is unavailable. Not a force
#                        switch: ignored when the dynamic binary can run.
#                        Also RPI_INSTALL_STATIC_FALLBACK=1.
#   --no-cache           Do not reuse cached partial/complete downloads.
#   --add-path           Add the install directory to the detected shell profile
#                        without prompting (also RPI_INSTALL_ADD_PATH=1)
#   --no-add-path        Never modify a shell profile (also
#                        RPI_INSTALL_ADD_PATH=0)
#   --musl               Force the musl target on Linux (auto-detected via ldd
#                        otherwise). On a glibc host the dynamic musl binary
#                        cannot run, so only the static fallback is usable.
#   -h, --help           Show usage
#
# L0 consent (native plugins are unsandboxed and run with your permissions):
# only an interactive prompt answer counts as consent — choosing 1/2/3 or
# pressing Enter for the default. A prompt timeout, a non-TTY run, or merely
# passing --profile/RPI_INSTALL_PROFILE does NOT consent: standard/full then
# fail before fetching the profile index or any artifact unless --yes /
# RPI_INSTALL_YES=1 is also given. mini never needs consent.
#
# Integrity and trust:
# - standard/full archives are pinned by the rpi-pages profile index
#   (${RPI_SITE_BASE_URL}/api/install-profiles/<ver>.json): the downloaded
#   archive sha256 must equal BOTH the Release sidecar and the index value.
#   A missing/invalid index fails closed; it never downgrades to mini.
# - mini and the static fallback use the existing Release sidecar model.
# - The sidecar is an integrity check only (corrupted downloads / mirror
#   mix-ups); it does NOT protect against tampered release assets. Artifact
#   signing is a planned follow-up (ADR-0011).
# - The bundle is unpacked and its plugins materialized locally (no plugin
#   network requests). Path traversal, links, unregistered members and
#   incompatible rpiAbi/minHostVersion entries are rejected.
#
# Environment overrides (mirrors and tests):
#   RPI_INSTALL_PROFILE           mini|standard|full (flags win)
#   RPI_INSTALL_YES               1/true/yes = non-interactive L0 consent
#   RPI_INSTALL_STATIC_FALLBACK   1/true/yes = consent to the static fallback
#   RPI_INSTALL_REQUIRE_PLUGINS   1/true/yes = exit 1 if any plugin fails
#   RPI_INSTALL_NO_CACHE          1/true/yes = disable the download cache
#   RPI_INSTALL_CACHE_DIR         cache root (default: XDG/macOS/~/.cache)
#   RPI_INSTALL_PROGRESS          auto|on|off|curl
#   RPI_INSTALL_ATTEMPTS          attempts per source (default 5)
#   RPI_INSTALL_STALL_TIMEOUT     seconds below 1 KiB/s before a retry (30)
#   RPI_INSTALL_CONNECT_TIMEOUT   connect timeout seconds (10)
#   RPI_INSTALL_PROMPT_TIMEOUT    profile prompt seconds (60)
#   RPI_INSTALL_HAS_LIBGCC        0/1 override for the musl dependency probe
#   RPI_INSTALL_HAS_LIBSTDCXX     0/1 override for the musl dependency probe
#   RPI_CODING_AGENT_DIR          agent dir (settings/extensions root)
#   RPI_INSTALL_ADD_PATH          1/0 (see --add-path/--no-add-path)
#   RPI_NONINTERACTIVE            1/true/yes = never prompt
#   RPI_GITHUB_API_URL            latest-release API
#   RPI_VERSION_CHECK_URL         fallback version endpoint
#   RPI_RELEASE_BASE_URL          GitHub release base URL
#   RPI_SITE_BASE_URL             official-site mirror base URL
#
# Download fallback order (ADR-0011 revision): 1) the browser_download_url
# from the GitHub API, 2) the naming-rule GitHub URL, 3) the official-site
# mirror. A failed DOWNLOAD moves to the next candidate; an integrity failure
# is always fatal and never switches sources.
#
# PATH behavior: when the install directory is not already the active `rpi`
# on PATH, an interactive run asks once (default: yes) before writing a marked
# `# rpi` block to the detected shell profile. The prompt requires stdout to
# be a terminal, a usable /dev/tty, and no CI/RPI_NONINTERACTIVE marker.
# --add-path / --no-add-path / RPI_INSTALL_ADD_PATH override it. Root,
# system-wide and outside-$HOME installs never edit a profile.

set -eu

REPO="revpidev/rpi"
GITHUB_API_URL=${RPI_GITHUB_API_URL:-"https://api.github.com/repos/$REPO/releases/latest"}
VERSION_CHECK_URL=${RPI_VERSION_CHECK_URL:-"https://revpi.dev/api/latest-version"}
RELEASE_BASE_URL=${RPI_RELEASE_BASE_URL:-"https://github.com/$REPO/releases"}
SITE_BASE_URL=${RPI_SITE_BASE_URL:-"https://revpi.dev"}

PREFIX=""
FORCE_MUSL=0
# Profile plugin sets. `scripts/profiles.json` is the packaging single source
# of truth; `scripts/test_package_profile.py` asserts these two lists match it
# (the installer cannot read JSON without extra dependencies).
RPI_PROFILE_STANDARD_PLUGINS='rpi-subagents rpi-todo rpi-ask-user-question rpi-smart-fetch rpi-statusline rpi-plan-mode'
RPI_PROFILE_FULL_PLUGINS="$RPI_PROFILE_STANDARD_PLUGINS rpi-usage"

PROFILE_FLAG=""
PROFILE=""
PROFILE_SOURCE=""
CONSENT=0
YES_FLAG=0
STATIC_FLAG=0
NO_CACHE_FLAG=0
PATH_ACTION=prompt
ADD_PATH_FLAG=0
NO_ADD_PATH_FLAG=0
TMPDIR_RPI=""
STAGE_BIN=""
STAGE_MANIFEST=""
READ_TIMEOUT_SUPPORTED=0
PROMPT_TIMEOUT=${RPI_INSTALL_PROMPT_TIMEOUT:-60}
CONNECT_TIMEOUT=${RPI_INSTALL_CONNECT_TIMEOUT:-10}
STALL_TIMEOUT=${RPI_INSTALL_STALL_TIMEOUT:-30}
MAX_ATTEMPTS=${RPI_INSTALL_ATTEMPTS:-5}
PROGRESS_MODE=${RPI_INSTALL_PROGRESS:-auto}

VERSION=""
API_BODY=""
RESOLVED_VIA=""
TARGET=""
IS_MUSL_TARGET=0
IS_STATIC=0
AGENT_DIR=""
PLUGIN_DIR=""
SETTINGS_FILE=""
ASSET_NAME=""
CANDIDATES=""
INDEX_SHA=""
EXPECTED_SHA=""
DOWNLOADED_FROM=""
CACHE_DIR=""
CACHE_PARTS=""
LOCK_PATH=""
HEARTBEAT_PID=""
PLUGINS_INSTALLED=0
PLUGINS_FAILED=0
PLUGINS_TOTAL=0
FAILED_PLUGIN_LINES=""

info() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

lower_trim() {
    printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//'
}

is_truthy() {
    case $(lower_trim "$1") in
        1 | true | yes | on) return 0 ;;
    esac
    return 1
}

# Strip trailing slashes (except the root itself) so HOME/ZDOTDIR/XDG values
# compare and join consistently.
normalize_dir() {
    _dir=$1
    while [ "${_dir%/}" != "$_dir" ] && [ "$_dir" != "/" ]; do
        _dir=${_dir%/}
    done
    printf '%s' "$_dir"
}

usage() {
    cat <<'EOF'
Usage: install.sh [options]

  --prefix <dir>       Install directory (default: ~/.local/bin)
  --profile <name>     mini|standard|full (default: standard)
  --yes                Non-interactive consent to the bundle's native plugins
  --static-fallback    Consent to the static musl fallback (only used when
                       the dynamic runtime is unavailable; not a force switch)
  --no-cache           Disable the download cache
  --add-path           Add the install directory to the shell profile
  --no-add-path        Never modify a shell profile
  --musl               Force the musl target on Linux
  -h, --help           Show this help

Profiles:
  mini       rpi binary only
  standard   rpi + 6 official plugins, one pinned archive (default)
  full       standard + rpi-usage

Only an interactive profile-prompt answer (or Enter for the default) counts as
consent to the native (L0) plugins. Non-interactive runs, prompt timeouts and
--profile/RPI_INSTALL_PROFILE alone need --yes / RPI_INSTALL_YES=1; otherwise
standard/full exit before fetching the profile index or any artifact. mini
needs no consent. CI should use RPI_INSTALL_PROFILE=mini or explicitly accept
the native-plugin risk with RPI_INSTALL_YES=1.

Environment overrides: RPI_INSTALL_PROFILE, RPI_INSTALL_YES,
RPI_INSTALL_STATIC_FALLBACK, RPI_INSTALL_REQUIRE_PLUGINS,
RPI_INSTALL_NO_CACHE, RPI_INSTALL_CACHE_DIR, RPI_INSTALL_PROGRESS,
RPI_INSTALL_ATTEMPTS, RPI_INSTALL_STALL_TIMEOUT,
RPI_INSTALL_CONNECT_TIMEOUT, RPI_INSTALL_PROMPT_TIMEOUT,
RPI_INSTALL_HAS_LIBGCC, RPI_INSTALL_HAS_LIBSTDCXX, RPI_CODING_AGENT_DIR,
RPI_INSTALL_ADD_PATH, RPI_NONINTERACTIVE, RPI_GITHUB_API_URL,
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
        --profile)
            [ $# -ge 2 ] || die "--profile requires mini, standard or full"
            PROFILE_FLAG=$2
            shift 2
            ;;
        --profile=*)
            PROFILE_FLAG=${1#--profile=}
            shift
            ;;
        --yes)
            YES_FLAG=1
            shift
            ;;
        --static-fallback)
            STATIC_FLAG=1
            shift
            ;;
        --no-cache)
            NO_CACHE_FLAG=1
            shift
            ;;
        --musl)
            FORCE_MUSL=1
            shift
            ;;
        --add-path)
            ADD_PATH_FLAG=1
            PATH_ACTION=add
            shift
            ;;
        --no-add-path)
            NO_ADD_PATH_FLAG=1
            PATH_ACTION=skip
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

if [ "$ADD_PATH_FLAG" = 1 ] && [ "$NO_ADD_PATH_FLAG" = 1 ]; then
    die "--add-path and --no-add-path are mutually exclusive"
fi

if [ -z "$PREFIX" ]; then
    [ -n "${HOME:-}" ] || die "\$HOME is not set; pass --prefix explicitly"
    PREFIX=$HOME/.local/bin
fi

HOME_DIR=""
if [ -n "${HOME:-}" ]; then
    HOME_DIR=$(normalize_dir "$HOME")
    [ "$HOME_DIR" != "/" ] || HOME_DIR=""
fi

# Flags win over the environment; the environment only supplies a default
# when neither --add-path nor --no-add-path was passed.
if [ "$PATH_ACTION" = prompt ]; then
    add_path_env=$(lower_trim "${RPI_INSTALL_ADD_PATH:-}")
    case $add_path_env in
        1 | true | yes | on) PATH_ACTION=add ;;
        0 | false | no | off) PATH_ACTION=skip ;;
    esac
fi

if is_truthy "${RPI_INSTALL_YES:-}"; then
    YES_FLAG=1
fi
if is_truthy "${RPI_INSTALL_STATIC_FALLBACK:-}"; then
    STATIC_FLAG=1
fi
if is_truthy "${RPI_INSTALL_NO_CACHE:-}"; then
    NO_CACHE_FLAG=1
fi

case $(lower_trim "$PROGRESS_MODE") in
    on | off | curl | auto) PROGRESS_MODE=$(lower_trim "$PROGRESS_MODE") ;;
    *) PROGRESS_MODE=auto ;;
esac
case $CONNECT_TIMEOUT in
    '' | *[!0-9]*) CONNECT_TIMEOUT=10 ;;
esac
case $STALL_TIMEOUT in
    '' | *[!0-9]*) STALL_TIMEOUT=30 ;;
esac
case $MAX_ATTEMPTS in
    '' | *[!0-9]* | 0) MAX_ATTEMPTS=5 ;;
esac
case $PROMPT_TIMEOUT in
    '' | *[!0-9]*) PROMPT_TIMEOUT=60 ;;
esac

# Feature-probe `read -t` once: bash/ksh/zsh support it, dash/POSIX sh does
# not. The prompt falls back to a normal blocking read on shells without it.
if (IFS= read -r -t 0 _ <<EOF
x
EOF
) 2>/dev/null; then
    READ_TIMEOUT_SUPPORTED=1
fi

cleanup() {
    [ -z "$HEARTBEAT_PID" ] || kill "$HEARTBEAT_PID" 2>/dev/null || true
    [ -z "$LOCK_PATH" ] || rm -f "$LOCK_PATH" 2>/dev/null || true
    [ -z "$STAGE_BIN" ] || rm -f "$STAGE_BIN" 2>/dev/null || true
    [ -z "$STAGE_MANIFEST" ] || rm -f "$STAGE_MANIFEST" 2>/dev/null || true
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

SHA256TOOL=""
if command -v sha256sum >/dev/null 2>&1; then
    SHA256TOOL=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    SHA256TOOL="shasum -a 256"
else
    die "neither sha256sum nor shasum found; cannot verify download integrity"
fi

sha256_file() { # sha256_file <file> -> hex
    if [ "$SHA256TOOL" = sha256sum ]; then
        sha256sum "$1" | sed 's/[[:space:]].*//'
    else
        shasum -a 256 "$1" | sed 's/[[:space:]].*//'
    fi
}

# Plain metadata fetch (no progress, no resume): version endpoints and the
# small profile index. Deliberately bounded by --max-time.
fetch_meta() { # fetch_meta <url> <output-file>
    if [ "$DL_TOOL" = curl ]; then
        curl -fsSL --connect-timeout "$CONNECT_TIMEOUT" --max-time 30 \
            --retry 2 -o "$2" "$1"
    else
        wget -q --timeout=10 --tries=2 -O "$2" "$1"
    fi
}

fetch_stdout() { # fetch_stdout <url>
    if [ "$DL_TOOL" = curl ]; then
        curl -fsSL --connect-timeout "$CONNECT_TIMEOUT" --max-time 30 \
            --retry 2 "$1"
    else
        wget -q --timeout=10 --tries=2 -O - "$1"
    fi
}

# ---- platform detection -----------------------------------------------------

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
                IS_MUSL_TARGET=1
            else
                TARGET=$arch-unknown-linux-gnu
            fi
            ;;
        *)
            die "unsupported operating system: $os (on Windows, use install.ps1)"
            ;;
    esac
}

# ---- version resolution -----------------------------------------------------

# Print every string value of JSON key $1, one per line. Deliberately
# dependency-free (no jq); release JSON never carries commas inside URLs.
json_values() {
    tr ',' '\n' | sed -n 's/^[^"]*"'"$1"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p'
}

# Best-effort asset size for the profile prompt (R3.11.4 says "when the
# metadata provides it"). The GitHub release JSON is normally pretty-printed,
# so look for "size" after the matching "name" key rather than assuming a
# single line/object. A miss only omits the size hint.
json_asset_size() { # json_asset_size <release-body> <asset-name>
    printf '%s\n' "$1" | awk -v needle="\"name\": \"$2\"" '
        index($0, needle) { found = 1 }
        found && match($0, /"size"[^0-9]*[0-9]+/) {
            s = substr($0, RSTART, RLENGTH)
            gsub(/[^0-9]/, "", s)
            print s
            exit
        }
    '
}

resolve_version() {
    if API_BODY=$(fetch_stdout "$GITHUB_API_URL" 2>/dev/null) && [ -n "$API_BODY" ]; then
        tag=$(printf '%s' "$API_BODY" | json_values tag_name | head -n 1)
        if [ -n "$tag" ]; then
            VERSION=${tag#v}
            RESOLVED_VIA="GitHub API"
            return 0
        fi
    fi
    warn "GitHub API unavailable; falling back to version endpoint"
    body=$(fetch_stdout "$VERSION_CHECK_URL" 2>/dev/null) ||
        die "version endpoint $VERSION_CHECK_URL is unreachable"
    VERSION=$(printf '%s' "$body" | json_values version | head -n 1)
    [ -n "$VERSION" ] || die "version endpoint $VERSION_CHECK_URL did not return a version"
    VERSION=${VERSION#v}
    RESOLVED_VIA="version endpoint"
    API_BODY=""
}

# ---- profile selection + L0 consent -----------------------------------------

plugins_for_profile() { # plugins_for_profile <profile>
    case $1 in
        mini) printf '%s' "" ;;
        standard) printf '%s' "$RPI_PROFILE_STANDARD_PLUGINS" ;;
        full) printf '%s' "$RPI_PROFILE_FULL_PLUGINS" ;;
    esac
}

plugins_multiline() { # plugins_multiline <space-list> -> " · "-joined
    _out=""
    for _p in $1; do
        if [ -z "$_out" ]; then _out=$_p; else _out="$_out · $_p"; fi
    done
    printf '%s' "$_out"
}

size_human() { # size_human <bytes> -> "33 MB"
    _bytes=$1
    case $_bytes in
        '' | *[!0-9]*) printf '%s' ""; return 0 ;;
    esac
    _mb=$((_bytes / 1048576))
    _frac=$(((_bytes % 1048576) * 10 / 1048576))
    printf '%s.%s MB' "$_mb" "$_frac"
}

can_prompt() {
    [ -t 1 ] || return 1
    [ -z "${CI:-}" ] || return 1
    is_truthy "${RPI_NONINTERACTIVE:-}" && return 1
    case ${TERM:-} in
        dumb) return 1 ;;
    esac
    ( : <>/dev/tty ) 2>/dev/null || return 1
    return 0
}

print_l0_plugins() { # print_l0_plugins <profile>
    info ""
    info "The '$1' profile installs these official plugins:"
    for _p in $(plugins_for_profile "$1"); do
        info "  - $_p"
    done
    info "Official plugins are native (L0): they run unsandboxed with your"
    info "permissions. Remove one later with \`rpi remove <name>\`."
}

prompt_profile() {
    _mini_size=""
    _std_size=""
    _full_size=""
    if [ -n "$API_BODY" ]; then
        _mini_size=$(json_asset_size "$API_BODY" "rpi-$VERSION-$TARGET.tar.gz")
        _std_size=$(json_asset_size "$API_BODY" "rpi-$VERSION-$TARGET-standard.tar.gz")
        _full_size=$(json_asset_size "$API_BODY" "rpi-$VERSION-$TARGET-full.tar.gz")
    fi
    _mini_note=""
    _std_note=""
    _full_note=""
    [ -z "$_mini_size" ] || _mini_note=" ($(size_human "$_mini_size"))"
    [ -z "$_std_size" ] || _std_note=" ($(size_human "$_std_size"))"
    [ -z "$_full_size" ] || _full_note=" ($(size_human "$_full_size"))"

    while :; do
        info ""
        info "Choose an install profile:"
        info "  1) mini      rpi core only — no plugins$_mini_note"
        info "  2) standard  rpi + 6 official plugins (recommended)$_std_note"
        info "               $(plugins_multiline "$RPI_PROFILE_STANDARD_PLUGINS")"
        info "  3) full      standard + usage$_full_note"
        info ""
        info "Everything above comes from ONE pinned archive — no plugin artifacts are"
        info "downloaded. The archive sha256 is pinned by the revpi.dev profile index."
        info "Official plugins are native (L0) and run unsandboxed with your permissions."
        info "Selecting standard/full here explicitly consents to that group. Remove one"
        info "later with \`rpi remove <name>\`."
        printf 'Profile [1/2/3, Enter = standard]: ' >&3
        _answer=""
        _read_ok=0
        if [ "$READ_TIMEOUT_SUPPORTED" = 1 ]; then
            if IFS= read -r -t "$PROMPT_TIMEOUT" _answer <&3; then
                _read_ok=1
            fi
        else
            if IFS= read -r _answer <&3; then
                _read_ok=1
            fi
        fi
        _answer=$(printf '%s' "$_answer" | tr -d '[:space:]')
        if [ "$_read_ok" = 0 ]; then
            info ""
            warn "profile prompt timed out; defaulting to standard WITHOUT plugin consent"
            PROFILE=standard
            PROFILE_SOURCE="timeout"
            CONSENT=0
            return 0
        fi
        case $_answer in
            '')
                PROFILE=standard
                PROFILE_SOURCE="prompt"
                CONSENT=1
                return 0
                ;;
            1 | mini)
                PROFILE=mini
                PROFILE_SOURCE="prompt"
                CONSENT=0
                return 0
                ;;
            2 | standard)
                PROFILE=standard
                PROFILE_SOURCE="prompt"
                CONSENT=1
                return 0
                ;;
            3 | full)
                PROFILE=full
                PROFILE_SOURCE="prompt"
                CONSENT=1
                return 0
                ;;
            *)
                warn "invalid choice: $_answer (expected 1, 2, 3, or Enter)"
                ;;
        esac
    done
}

select_profile() {
    if [ -n "$PROFILE_FLAG" ]; then
        PROFILE=$(lower_trim "$PROFILE_FLAG")
    elif [ -n "${RPI_INSTALL_PROFILE:-}" ]; then
        PROFILE=$(lower_trim "$RPI_INSTALL_PROFILE")
    fi
    case $PROFILE in
        mini | standard | full) ;;
        '')
            PROFILE=""
            ;;
        *)
            die "invalid profile: $PROFILE (expected mini, standard or full)"
            ;;
    esac

    if [ -n "$PROFILE" ]; then
        PROFILE_SOURCE="flag/env"
        if [ "$YES_FLAG" = 1 ] && [ "$PROFILE" != mini ]; then
            # --yes already grants consent; do not ask twice.
            CONSENT=1
        elif can_prompt; then
            # An explicit choice is still honored; consent comes only from an
            # interactive confirmation of the native-plugin group.
            if [ "$PROFILE" != mini ]; then
                print_l0_plugins "$PROFILE"
                info ""
                exec 3<>/dev/tty || die "cannot open /dev/tty for consent"
                printf "Install these native plugins with profile '$PROFILE'? [y/N] " >&3
                _answer=""
                if [ "$READ_TIMEOUT_SUPPORTED" = 1 ]; then
                    IFS= read -r -t "$PROMPT_TIMEOUT" _answer <&3 || _answer="n"
                else
                    IFS= read -r _answer <&3 || _answer="n"
                fi
                exec 3>&-
                case $(lower_trim "$_answer") in
                    y | yes) CONSENT=1 ;;
                    *) CONSENT=0 ;;
                esac
            else
                info "profile: mini (no native plugins)"
            fi
        fi
    else
        if can_prompt; then
            exec 3<>/dev/tty || die "cannot open /dev/tty for the profile prompt"
            prompt_profile
            exec 3>&-
        else
            PROFILE=standard
            PROFILE_SOURCE="noninteractive"
            CONSENT=0
        fi
    fi

    if [ "$PROFILE" != mini ] && [ "$CONSENT" != 1 ]; then
        if [ "$YES_FLAG" = 1 ]; then
            CONSENT=1
        else
            print_l0_plugins "$PROFILE" >&2
            die "refusing to install native plugins without consent (profile '$PROFILE', source: $PROFILE_SOURCE).
Choose one of:
  - re-run interactively and answer the profile prompt,
  - add --yes (or RPI_INSTALL_YES=1) to accept the official native plugins,
  - use --profile mini (no plugins).
Nothing was downloaded."
        fi
    fi
    if [ "$PROFILE" != mini ]; then
        info "profile: $PROFILE (native-plugin consent: yes)"
    fi
}

# ---- musl runtime probe and static fallback ---------------------------------

host_is_musl() {
    if [ "$FORCE_MUSL" = 1 ] && [ "$(uname -s)" = Linux ]; then
        # Check the actual loader rather than the flag: a glibc host cannot
        # run a dynamically linked musl binary even with --musl.
        :
    fi
    for _loader in /lib/ld-musl-*.so.1; do
        [ -e "$_loader" ] && return 0
    done
    return 1
}

has_library() { # has_library <override-env-value> <soname>
    _override=$1
    _soname=$2
    case $(lower_trim "$_override") in
        1 | true | yes) return 0 ;;
        0 | false | no) return 1 ;;
    esac
    if command -v ldconfig >/dev/null 2>&1; then
        if ldconfig -p 2>/dev/null | grep -F "$_soname" >/dev/null 2>&1; then
            return 0
        fi
    fi
    for _dir in /usr/lib /lib /usr/local/lib /usr/lib64 /lib64 \
        /usr/lib/x86_64-linux-musl /lib/x86_64-linux-musl \
        /usr/lib/aarch64-linux-musl /lib/aarch64-linux-musl; do
        [ -e "$_dir/$_soname" ] && return 0
    done
    return 1
}

# Collect native extension directories that the loader may scan. Global,
# project, and settings-listed paths (global + project settings).
native_scan_roots() {
    _agent=${RPI_CODING_AGENT_DIR:-${HOME:-}/.rpi/agent}
    [ -n "${HOME:-}" ] || _agent=${RPI_CODING_AGENT_DIR:-}
    if [ -n "$_agent" ]; then
        printf '%s\n' "$_agent/extensions"
        if [ -f "$_agent/settings.json" ]; then
            settings_extension_paths "$_agent/settings.json" "$_agent"
        fi
    fi
    printf '%s\n' "$(pwd)/.rpi/extensions"
    if [ -f "$(pwd)/.rpi/settings.json" ]; then
        settings_extension_paths "$(pwd)/.rpi/settings.json" "$(pwd)"
    fi
}

settings_extension_paths() { # settings_extension_paths <settings.json> <base-dir>
    _file=$1
    _base=$2
    _array=$(tr '\n' ' ' <"$_file" 2>/dev/null |
        sed -n 's/.*"extensions"[[:space:]]*:[[:space:]]*\[\([^]]*\)\].*/\1/p' | head -n 1)
    [ -n "$_array" ] || return 0
    printf '%s\n' "$_array" | tr ',' '\n' |
        sed -n 's/^[[:space:]]*"\([^"]*\)".*/\1/p' |
        while IFS= read -r _entry; do
            [ -n "$_entry" ] || continue
            case $_entry in
                /*) printf '%s\n' "$_entry" ;;
                *) printf '%s\n' "$_base/$_entry" ;;
            esac
        done
}

# A settings `extensions` entry may name a file directly; a native carrier
# there is just as loadable as one inside a plugin directory.
path_is_native_extension_file() { # path_is_native_extension_file <path>
    case $1 in
        *.so | *.dylib | *.dll) return 0 ;;
    esac
    return 1
}

# A directory is native if one of its plugin entries resolves a `native`
# carrier and no `wasm` carrier wins (docs/extension-abi.md §5: wasm wins).
dir_has_native_extension() { # dir_has_native_extension <dir> -> prints path
    _root=$1
    [ -d "$_root" ] || return 1
    _found=""
    for _entry in "$_root"/*; do
        [ -e "$_entry" ] || continue
        _base=${_entry##*/}
        case $_base in
            .tmp-* | .old-* | *".tmp-"* | *".old-"*) continue ;;
        esac
        if [ -d "$_entry" ]; then
            _manifest=$_entry/rpi-extension.json
            _wasm=""
            _native=""
            if [ -f "$_manifest" ]; then
                _wasm=$(sed -n 's/.*"wasm"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_manifest" | head -n 1)
                _native=$(sed -n 's/.*"native"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_manifest" | head -n 1)
            fi
            if [ -n "$_wasm" ] && [ -f "$_entry/$_wasm" ]; then
                continue
            fi
            if [ -n "$_native" ] && [ -f "$_entry/$_native" ]; then
                _found="$_entry"
                break
            fi
            if [ -f "$_entry/index.wasm" ]; then
                continue
            fi
        else
            case $_base in
                *.so | *.dylib | *.dll)
                    _found=$_entry
                    break
                    ;;
            esac
        fi
    done
    [ -n "$_found" ] || return 1
    printf '%s' "$_found"
}

precheck_existing_native_plugins() {
    _found_lines=""
    _roots=$(native_scan_roots)
    _old_ifs=$IFS
    IFS='
'
    for _root in $_roots; do
        [ -n "$_root" ] || continue
        if [ -e "$_root" ] && [ ! -r "$_root" ]; then
            IFS=$_old_ifs
            die "cannot read extension path $_root while checking for native plugins; refusing the static fallback"
        fi
        if [ -f "$_root" ]; then
            if path_is_native_extension_file "$_root"; then
                _found_lines="$_found_lines$_root
"
            fi
            continue
        fi
        if _hit=$(dir_has_native_extension "$_root"); then
            _found_lines="$_found_lines$_hit
"
        fi
    done
    IFS=$_old_ifs
    if [ -n "$_found_lines" ]; then
        warn "existing native plugins block the static fallback:"
        printf '%s' "$_found_lines" | while IFS= read -r _line; do
            [ -n "$_line" ] || continue
            warn "  $_line"
        done
        die "the static build cannot load native plugins. Move those directories away first (nothing was deleted); if the current rpi still starts, \`rpi --no-extensions\` can be used temporarily."
    fi
}

choose_static_fallback() {
    # Only a missing dynamic runtime can trigger the fallback; --static-fallback
    # is consent, never a force switch.
    dynamic_ok=0
    if [ "$IS_MUSL_TARGET" = 1 ]; then
        if host_is_musl && has_library "${RPI_INSTALL_HAS_LIBGCC:-}" libgcc_s.so.1; then
            if [ "$PROFILE" = mini ] || has_library "${RPI_INSTALL_HAS_LIBSTDCXX:-}" libstdc++.so.6; then
                dynamic_ok=1
            fi
        fi
    else
        dynamic_ok=1
    fi

    if [ "$dynamic_ok" = 1 ]; then
        if [ "$STATIC_FLAG" = 1 ]; then
            warn "--static-fallback/RPI_INSTALL_STATIC_FALLBACK is only consent for a missing dynamic runtime; the dynamic build is usable, so the fallback is ignored"
        fi
        IS_STATIC=0
        return 0
    fi

    # Dynamic runtime unavailable. Explain exactly what is missing.
    missing=""
    if [ "$IS_MUSL_TARGET" = 1 ] && ! host_is_musl; then
        missing="a musl dynamic loader (/lib/ld-musl-*.so.1)"
    fi
    if [ -z "$missing" ] && ! has_library "${RPI_INSTALL_HAS_LIBGCC:-}" libgcc_s.so.1; then
        missing="libgcc_s.so.1"
    fi
    if [ -z "$missing" ] && [ "$PROFILE" != mini ] &&
        ! has_library "${RPI_INSTALL_HAS_LIBSTDCXX:-}" libstdc++.so.6; then
        missing="libstdc++.so.6 (needed by the standard/full plugins)"
    fi
    [ -n "$missing" ] || missing="the dynamic musl runtime"

    info ""
    warn "the dynamic rpi build cannot run on this system: missing $missing"
    warn "install the runtime dependencies and re-run:  apk add libgcc libstdc++"
    if [ "$PROFILE" != mini ]; then
        warn "or install without plugins:  --profile mini"
    fi

    consent=0
    if [ "$STATIC_FLAG" = 1 ]; then
        consent=1
    elif can_prompt; then
        exec 3<>/dev/tty || consent=0
        if [ -n "${READ_TIMEOUT_SUPPORTED:-}" ] && [ "$READ_TIMEOUT_SUPPORTED" = 1 ]; then
            printf "Install the static fallback instead (binary only, no plugins)? [y/N] " >&3
            _answer=""
            IFS= read -r -t "$PROMPT_TIMEOUT" _answer <&3 || _answer="n"
        else
            printf "Install the static fallback instead (binary only, no plugins)? [y/N] " >&3
            _answer=""
            IFS= read -r _answer <&3 || _answer="n"
        fi
        exec 3>&-
        case $(lower_trim "$_answer") in
            y | yes) consent=1 ;;
        esac
    fi

    if [ "$consent" != 1 ]; then
        die "refusing to fall back to the static build without consent. Re-run with --static-fallback (or RPI_INSTALL_STATIC_FALLBACK=1) after deciding to use the plugin-less static build, or install the dependencies above."
    fi
    precheck_existing_native_plugins
    IS_STATIC=1
    warn "using the static musl fallback: this build cannot load native plugins"
}

# ---- profile index ----------------------------------------------------------

# The index is a flat, asset-per-line JSON document (rpi-docs/install-packaging
# §3). Validate structure and the exact asset entry without jq.
read_profile_index() { # read_profile_index <version> <asset> -> sha
    _url=$SITE_BASE_URL/api/install-profiles/$1.json
    _file=$TMPDIR_RPI/profile-index.json
    fetch_meta "$_url" "$_file" || {
        warn "profile index unavailable: $_url"
        die "profile index unavailable for v$1; refusing to install without the bundle hash anchor.
Fix/retry the site index, or install without plugins with --profile mini."
    }
    [ -s "$_file" ] || die "profile index is empty: $_url"
    _size=$(wc -c <"$_file" | tr -d ' ')
    [ "$_size" -le 262144 ] || die "profile index too large ($_size bytes): $_url"
    head -c 1 "$_file" | grep -q '{' ||
        die "profile index is not a JSON object: $_url (use --profile mini if the site is broken)"

    _schema_count=$(grep -c '"schemaVersion"' "$_file" || true)
    [ "$_schema_count" = 1 ] || die "profile index must contain schemaVersion exactly once (found $_schema_count)"
    _schema=$(sed -n 's/.*"schemaVersion"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$_file" | head -n 1)
    [ "$_schema" = 1 ] || die "unsupported profile index schemaVersion: ${_schema:-missing}"

    _version_count=$(grep -c '"version"' "$_file" || true)
    [ "$_version_count" = 1 ] || die "profile index must contain version exactly once (found $_version_count)"
    _index_version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$_file" | head -n 1)
    [ "$_index_version" = "$1" ] ||
        die "profile index version mismatch: index=$_index_version expected=$1"

    grep -q '"assets"[[:space:]]*:[[:space:]]*{' "$_file" ||
        die "profile index has no assets object"

    _asset_count=$(grep -F "\"$2\"" "$_file" | grep -c '"[0-9a-fA-F]\{64\}"' || true)
    [ "$_asset_count" = 1 ] ||
        die "profile index asset entry missing/duplicated for $2 (found $_asset_count)"
    _sha=$(grep -F "\"$2\"" "$_file" |
        sed -n 's/^[[:space:]]*"'"$2"'"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F]\{64\}\)"[[:space:]]*,\{0,1\}[[:space:]]*$/\1/p')
    [ -n "$_sha" ] || die "profile index entry for $2 is not a 64-hex sha256"
    INDEX_SHA=$(printf '%s' "$_sha" | tr '[:upper:]' '[:lower:]')
}

# ---- asset names / candidates ----------------------------------------------

build_asset_name() {
    if [ "$IS_STATIC" = 1 ]; then
        ASSET_NAME=rpi-$VERSION-$TARGET-static.tar.gz
    elif [ "$PROFILE" = mini ]; then
        ASSET_NAME=rpi-$VERSION-$TARGET.tar.gz
    else
        ASSET_NAME=rpi-$VERSION-$TARGET-$PROFILE.tar.gz
    fi
}

add_candidate() {
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

build_candidates() {
    if [ -n "$API_BODY" ]; then
        for _url in $(printf '%s' "$API_BODY" | json_values browser_download_url); do
            case $_url in
                */"$ASSET_NAME") add_candidate "$_url" ;;
            esac
        done
    fi
    add_candidate "$RELEASE_BASE_URL/download/v$VERSION/$ASSET_NAME"
    add_candidate "$SITE_BASE_URL/releases/download/v$VERSION/$ASSET_NAME"
}

# ---- download cache / locking / resumable transfer --------------------------

resolve_cache_dir() {
    if [ "$NO_CACHE_FLAG" = 1 ]; then
        CACHE_DIR=$TMPDIR_RPI/cache
    elif [ -n "${RPI_INSTALL_CACHE_DIR:-}" ]; then
        CACHE_DIR=$RPI_INSTALL_CACHE_DIR
    elif [ -n "${XDG_CACHE_HOME:-}" ]; then
        CACHE_DIR=$XDG_CACHE_HOME/rpi/install
    elif [ "$(uname -s)" = Darwin ] && [ -n "${HOME:-}" ]; then
        CACHE_DIR=$HOME/Library/Caches/rpi/install
    elif [ -n "${HOME:-}" ]; then
        CACHE_DIR=$HOME/.cache/rpi/install
    else
        CACHE_DIR=$TMPDIR_RPI/cache
    fi
    CACHE_PARTS=$CACHE_DIR/parts
    mkdir -p "$CACHE_PARTS" 2>/dev/null || {
        CACHE_DIR=$TMPDIR_RPI/cache
        CACHE_PARTS=$CACHE_DIR/parts
        mkdir -p "$CACHE_PARTS"
    }
    gc_cache
}

# Seven-day GC: active transfers hold a fresh lock and are skipped; anything
# else old enough is discarded (a cache miss just re-downloads).
gc_cache() {
    [ "$NO_CACHE_FLAG" = 1 ] && return 0
    _now=$(date +%s)
    for _f in "$CACHE_PARTS"/*; do
        [ -e "$_f" ] || continue
        _base=${_f##*/}
        case $_base in
            *.lock) continue ;;
        esac
        if [ -f "$CACHE_PARTS/$_base.lock" ]; then
            continue
        fi
        _mt=$(mtime_of "$_f" 2>/dev/null || echo "$_now")
        if [ $((_now - _mt)) -gt 604800 ]; then
            rm -f "$_f"
        fi
    done
}

mtime_of() {
    if stat -c %Y "$1" >/dev/null 2>&1; then
        stat -c %Y "$1"
    else
        stat -f %m "$1"
    fi
}

lock_is_stale() { # lock_is_stale <lock>
    _mt=$(mtime_of "$1")
    _now=$(date +%s)
    [ $((_now - _mt)) -gt 3600 ]
}

acquire_lock() { # acquire_lock <lock>
    while :; do
        if (set -C; : >"$1") 2>/dev/null; then
            LOCK_PATH=$1
            return 0
        fi
        if [ -f "$1" ] && lock_is_stale "$1"; then
            warn "reclaiming a stale download lock: $1"
            rm -f "$1"
            continue
        fi
        warn "another installer is downloading this asset ($1); waiting..."
        sleep 2
    done
}

start_heartbeat() {
    ( while :; do
        sleep 30
        [ -n "$LOCK_PATH" ] && [ -f "$LOCK_PATH" ] || exit 0
        touch "$LOCK_PATH" 2>/dev/null || exit 0
    done ) &
    HEARTBEAT_PID=$!
}

stop_heartbeat_and_lock() {
    [ -z "$HEARTBEAT_PID" ] || kill "$HEARTBEAT_PID" 2>/dev/null || true
    HEARTBEAT_PID=""
    [ -z "$LOCK_PATH" ] || rm -f "$LOCK_PATH" 2>/dev/null || true
    LOCK_PATH=""
}

# meta file: key=value lines with the expected sha and response validators.
meta_value() { # meta_value <file> <key>
    sed -n 's/^'"$2"'=//p' "$1" 2>/dev/null | head -n 1
}

# Persist the response validators as soon as a transfer has produced bytes:
# the next attempt (retry or mirror switch) can then send Range + If-Range and
# keep the partial instead of restarting from zero. Writing this only after a
# *successful* transfer would make every interrupted first download start over.
write_asset_meta() { # <meta> <sha> <asset> <etag> <lastmod> <url>
    printf 'sha=%s\nasset=%s\nversion=%s\netag=%s\nlast_modified=%s\nurl=%s\n' \
        "$2" "$3" "$VERSION" "$4" "$5" "$6" >"$1"
}

response_code() { # response_code <header-file>
    awk '/^HTTP\//{code=$2} END{print code}' "$1" 2>/dev/null
}

response_header() { # response_header <header-file> <name>
    awk -v name="$2" '
        BEGIN { IGNORECASE = 1 }
        index(tolower($0), tolower(name) ":") == 1 {
            sub(/^[^:]*:[[:space:]]*/, ""); gsub(/\r/, "");
            value = $0
        }
        END { print value }
    ' "$1" 2>/dev/null
}

progress_total() { # progress_total <headers> <offset> <part-size>
    _len=$(response_header "$1" Content-Length)
    case $_len in '' | *[!0-9]*) _len=0 ;; esac
    if [ "$_len" -gt 0 ]; then
        printf '%s' $((_len + $2))
    else
        printf '%s' 0
    fi
}

print_progress() { # print_progress <downloaded> <total> <offset>
    _done=$1
    _total=$2
    if [ "$_total" -gt 0 ]; then
        _pct=$(((_done) * 100 / _total))
        [ "$_pct" -gt 100 ] && _pct=100
    else
        _pct=""
    fi
    if [ "$PROGRESS_MODE" != off ] && [ -t 2 ] && [ "$PROGRESS_MODE" != curl ]; then
        printf '\r\033[2K  %s%%  %s / %s' "${_pct:-?}" \
            "$(size_human "$_done")" "$(size_human "$_total")" >&2
    elif [ "$PROGRESS_MODE" != off ] && [ -t 2 ]; then
        # curl draws its own single-line bar.
        :
    else
        # Non-TTY: 25/50/75/100% marks, no ANSI, no carriage returns.
        [ -n "$_pct" ] || return 0
        for _mark in 25 50 75 100; do
            if [ "$_pct" -ge "$_mark" ] && [ "${PROGRESS_LAST_MARK:-0}" -lt "$_mark" ]; then
                printf 'progress: %s%%\n' "$_mark" >&2
                PROGRESS_LAST_MARK=$_mark
            fi
        done
    fi
}

transfer_once() { # transfer_once <url> <part> <headers> <offset> <validator>
    _url=$1
    _part=$2
    _headers=$3
    _offset=$4
    _validator=$5
    : >"$_headers"
    if [ "$DL_TOOL" = wget ]; then
        # wget has no If-Range; never append to bytes of unknown provenance.
        [ "$_offset" -eq 0 ] || rm -f "$_part"
        wget -q --timeout="$STALL_TIMEOUT" --tries=1 -O "$_part" "$_url" 2>/dev/null
        return $?
    fi
    if [ "$_offset" -gt 0 ] && [ -n "$_validator" ]; then
        curl -fsSL -C - -D "$_headers" -o "$_part" \
            --connect-timeout "$CONNECT_TIMEOUT" \
            --speed-limit 1024 --speed-time "$STALL_TIMEOUT" \
            -H "If-Range: $_validator" "$_url"
        return $?
    fi
    curl -fsSL -D "$_headers" -o "$_part" \
        --connect-timeout "$CONNECT_TIMEOUT" \
        --speed-limit 1024 --speed-time "$STALL_TIMEOUT" "$_url"
}

# Download one asset with resume, progress, retries and mirror fallback.
# Sets DOWNLOADED_FROM or dies (integrity failures are fatal).
download_asset() { # download_asset <asset> <candidates-newline-list> <expected-sha>
    _asset=$1
    _cands=$2
    _expected=$3
    _part=$CACHE_PARTS/$_asset
    _meta=$CACHE_PARTS/$_asset.meta
    _hdr=$CACHE_PARTS/$_asset.hdr
    _sentinel=$CACHE_PARTS/$_asset.sentinel
    _lock=$CACHE_PARTS/$_asset.lock

    # A complete cached file with the same expected hash short-circuits all
    # network work (and keeps re-runs cheap).
    if [ -f "$_part" ] && [ -f "$_meta" ] &&
        [ "$(meta_value "$_meta" sha)" = "$_expected" ] &&
        [ "$(sha256_file "$_part")" = "$_expected" ]; then
        info "using cached $_asset ($_part)"
        DOWNLOADED_FROM=cache
        return 0
    fi

    # meta from another release/asset (or an older index anchor) is unusable.
    if [ -f "$_meta" ] && { [ "$(meta_value "$_meta" sha)" != "$_expected" ] ||
        [ "$(meta_value "$_meta" asset)" != "$_asset" ] ||
        [ "$(meta_value "$_meta" version)" != "$VERSION" ]; }; then
        rm -f "$_part" "$_meta"
    fi

    acquire_lock "$_lock"
    start_heartbeat

    _ifs=$IFS
    IFS='
'
    for _url in $_cands; do
        IFS=$_ifs
        [ -n "$_url" ] || continue
        _attempt=1
        while [ "$_attempt" -le "$MAX_ATTEMPTS" ]; do
            _offset=0
            [ -f "$_part" ] && _offset=$(wc -c <"$_part" | tr -d ' ')
            _validator=""
            if [ "$_offset" -gt 0 ]; then
                _validator=$(meta_value "$_meta" etag)
                [ -n "$_validator" ] || _validator=$(meta_value "$_meta" last_modified)
                if [ -z "$_validator" ]; then
                    # No validator means we cannot prove the partial bytes
                    # belong to this entity; start over rather than append.
                    rm -f "$_part" "$_meta"
                    _offset=0
                fi
            fi
            info "downloading $_url (attempt $_attempt/$MAX_ATTEMPTS)"
            rm -f "$_sentinel"
            ( set +e
              transfer_once "$_url" "$_part" "$_hdr" "$_offset" "$_validator"
              _transfer_rc=$?
              printf '%s\n' "$_transfer_rc" >"$_sentinel" ) &
            _pid=$!
            PROGRESS_LAST_MARK=0
            while [ ! -f "$_sentinel" ]; do
                _now_size=0
                [ -f "$_part" ] && _now_size=$(wc -c <"$_part" | tr -d ' ')
                _total=$(progress_total "$_hdr" "$_offset" "$_now_size")
                print_progress "$_now_size" "$_total" "$_offset"
                sleep 1
            done
            wait "$_pid" 2>/dev/null || true
            _code=$(cat "$_sentinel" 2>/dev/null || echo 1)
            rm -f "$_sentinel"
            _status=$(response_code "$_hdr")
            _etag=$(response_header "$_hdr" ETag)
            _lastmod=$(response_header "$_hdr" Last-Modified)
            # Keep the validators with any bytes we already have so an
            # interrupted transfer resumes on the next attempt/mirror.
            if { [ "$_status" = 200 ] || [ "$_status" = 206 ]; } &&
                [ -f "$_part" ] && [ -s "$_part" ]; then
                write_asset_meta "$_meta" "$_expected" "$_asset" "$_etag" "$_lastmod" "$_url"
            fi

            if [ "$_status" = 200 ] && [ "$_offset" -gt 0 ]; then
                warn "server ignored the range request; restarting from zero (discarded $_offset bytes)"
                rm -f "$_part" "$_meta"
                _attempt=$((_attempt + 1))
                continue
            fi
            if [ "$_status" = 206 ] && [ "$_offset" -gt 0 ]; then
                _resp_validator=$_etag
                [ -n "$_resp_validator" ] || _resp_validator=$_lastmod
                if [ -n "$_validator" ] && [ -n "$_resp_validator" ] &&
                    [ "$_validator" != "$_resp_validator" ]; then
                    warn "resume validator changed; restarting from zero"
                    rm -f "$_part" "$_meta"
                    _attempt=$((_attempt + 1))
                    continue
                fi
            fi
            if [ "$_status" = 416 ]; then
                if [ -f "$_part" ] && [ "$(sha256_file "$_part")" = "$_expected" ]; then
                    _code=0
                else
                    warn "server rejected the resume range; restarting from zero"
                    rm -f "$_part" "$_meta"
                    _attempt=$((_attempt + 1))
                    continue
                fi
            fi
            if [ "$_code" = 0 ]; then
                [ -s "$_part" ] || warn "empty download from $_url"
                if [ -s "$_part" ] && ! head -c 256 "$_part" | grep -qi '<html'; then
                    write_asset_meta "$_meta" "$_expected" "$_asset" "$_etag" "$_lastmod" "$_url"
                    DOWNLOADED_FROM=$_url
                    stop_heartbeat_and_lock
                    IFS=$_ifs
                    return 0
                fi
                warn "unexpected HTML/empty response from $_url"
            fi

            case $_status in
                403 | 404)
                    warn "download failed from $_url (HTTP $_status); trying the next mirror"
                    break
                    ;;
                429 | 503)
                    _retry_after=$(response_header "$_hdr" Retry-After)
                    case $_retry_after in
                        '' | *[!0-9]*) _sleep=$((1 << (_attempt - 1))) ;;
                        *) _sleep=$_retry_after ;;
                    esac
                    [ "$_sleep" -gt 60 ] && _sleep=60
                    warn "HTTP $_status; retrying in ${_sleep}s"
                    sleep "$_sleep"
                    ;;
                *)
                    if [ "$_attempt" -ge "$MAX_ATTEMPTS" ]; then
                        warn "download failed from $_url after $_attempt attempts; trying the next mirror"
                        break
                    fi
                    _sleep=$((1 << (_attempt - 1)))
                    [ "$_sleep" -gt 8 ] && _sleep=8
                    warn "download failed from $_url (HTTP ${_status:-?}); retrying in ${_sleep}s"
                    sleep "$_sleep"
                    ;;
            esac
            _attempt=$((_attempt + 1))
        done
        IFS='
'
    done
    IFS=$_ifs
    stop_heartbeat_and_lock
    die "all download candidates failed for $_asset (tried: $(printf '%s' "$_cands" | tr '\n' ' '))"
}

# ---- outer archive integrity + safe extraction ------------------------------

verify_outer_integrity() {
    _actual=$(sha256_file "$CACHE_PARTS/$ASSET_NAME")
    if [ "$_actual" != "$EXPECTED_SHA" ]; then
        rm -f "$CACHE_PARTS/$ASSET_NAME" "$CACHE_PARTS/$ASSET_NAME.meta"
        die "sha256 integrity check failed for $ASSET_NAME (expected $EXPECTED_SHA, got $_actual); aborting without switching sources"
    fi
    if [ -n "$INDEX_SHA" ] && [ "$_actual" != "$INDEX_SHA" ]; then
        rm -f "$CACHE_PARTS/$ASSET_NAME" "$CACHE_PARTS/$ASSET_NAME.meta"
        die "downloaded $ASSET_NAME does not match the rpi-pages profile index (index=$INDEX_SHA, archive=$_actual); aborting without switching sources"
    fi
    info "sha256 integrity check passed"
}

# Reject unsafe names, links, devices and unregistered members before any
# extraction writes bytes.
validate_outer_archive() { # validate_outer_archive <archive>
    _archive=$1
    [ -s "$_archive" ] || die "empty archive: $_archive"
    if ! tar -tzf "$_archive" >"$TMPDIR_RPI/archive-members.raw" 2>/dev/null; then
        die "cannot read archive listing: $_archive"
    fi
    # Normalize a leading "./" once so every later check compares the same
    # names (our packaging emits bare names).
    sed 's#^\./##' "$TMPDIR_RPI/archive-members.raw" >"$TMPDIR_RPI/archive-members"
    # Types: regular files and directories only (no symlinks/hardlinks).
    tar -tvzf "$_archive" 2>/dev/null | cut -c1 | sort -u >"$TMPDIR_RPI/archive-types"
    while IFS= read -r _type; do
        case $_type in
            - | d) ;;
            *) die "archive contains an unsupported member type '$_type' (links/devices are rejected)" ;;
        esac
    done <"$TMPDIR_RPI/archive-types"

    _expected_plugins=""
    if [ "$PROFILE" != mini ] && [ "$IS_STATIC" != 1 ]; then
        _expected_plugins=$(plugins_for_profile "$PROFILE")
    fi
    _seen_plugin_files=""
    while IFS= read -r _name; do
        [ -n "$_name" ] || continue
        case $_name in
            ./*) _name=${_name#./} ;;
        esac
        case $_name in
            '' | /* | .. | ../* | */.. | */../*)
                die "unsafe archive member path: $_name"
                ;;
        esac
        case $_name in
            rpi)
                :
                ;;
            extensions | extensions/)
                [ "$PROFILE" != mini ] && [ "$IS_STATIC" != 1 ] ||
                    die "unexpected extension directory in a plugin-less archive"
                ;;
            settings.defaults.json)
                [ "$PROFILE" != mini ] && [ "$IS_STATIC" != 1 ] ||
                    die "unexpected settings.defaults.json in a $PROFILE archive"
                ;;
            extensions/*)
                [ "$PROFILE" != mini ] && [ "$IS_STATIC" != 1 ] ||
                    die "unexpected plugin member in a plugin-less archive: $_name"
                _base=${_name#extensions/}
                case $_base in
                    */*) die "nested extension path: $_name" ;;
                esac
                case $_base in
                    *.rpix) _plugin=${_base%-$VERSION-$TARGET.rpix} ;;
                    *.rpix.sha256) _plugin=${_base%-$VERSION-$TARGET.rpix.sha256} ;;
                    *) die "unregistered extension member: $_name" ;;
                esac
                _found=0
                for _p in $_expected_plugins; do
                    [ "$_p" = "$_plugin" ] && _found=1
                done
                [ "$_found" = 1 ] || die "unregistered plugin in archive: $_name"
                case " $_seen_plugin_files " in
                    *" $_base "*) die "duplicate archive member: $_name" ;;
                esac
                _seen_plugin_files="$_seen_plugin_files $_base"
                ;;
            *)
                die "unregistered archive member: $_name"
                ;;
        esac
    done <"$TMPDIR_RPI/archive-members"

    # Each expected plugin must have exactly one rpix + sidecar pair.
    for _p in $_expected_plugins; do
        grep -Fxq "extensions/$_p-$VERSION-$TARGET.rpix" "$TMPDIR_RPI/archive-members" ||
            die "archive is missing extensions/$_p-$VERSION-$TARGET.rpix"
        grep -Fxq "extensions/$_p-$VERSION-$TARGET.rpix.sha256" "$TMPDIR_RPI/archive-members" ||
            die "archive is missing the sidecar for $_p"
    done
    if [ "$IS_STATIC" != 1 ] && [ "$PROFILE" != mini ]; then
        grep -Fxq "settings.defaults.json" "$TMPDIR_RPI/archive-members" ||
            die "archive is missing settings.defaults.json"
    fi
}
extract_outer_archive() { # extract_outer_archive <archive> <dir>
    mkdir -p "$2"
    tar -xzf "$1" -C "$2" || die "failed to extract archive: $1"
}

# ---- atomic binary install --------------------------------------------------

install_binary() { # install_binary <extracted-dir> <binary-name>
    _src=$1/$2
    [ -f "$_src" ] || die "archive did not contain $_src"
    [ -d "$PREFIX" ] || mkdir -p "$PREFIX" 2>/dev/null || sudo_hint
    [ -w "$PREFIX" ] || sudo_hint

    _stage_bin=$PREFIX/.rpi.stage-$$
    _stage_manifest=$PREFIX/.rpi.install.json.stage-$$
    STAGE_BIN=$_stage_bin
    STAGE_MANIFEST=$_stage_manifest
    cp "$_src" "$_stage_bin" || { rm -f "$_stage_bin"; sudo_hint; }
    chmod 755 "$_stage_bin" || die "failed to chmod 755 $_stage_bin"

    PREFIX_ABS=$(cd "$PREFIX" && pwd)
    INSTALL_PATH=$PREFIX_ABS/rpi
    MANIFEST=$PREFIX_ABS/rpi.install.json

    # Upgrade messaging: read the existing manifest (if any) before replacing
    # anything, and keep the old binary/manifest untouched on any earlier fatal.
    if [ -f "$MANIFEST" ]; then
        _current=$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$MANIFEST" | head -n 1)
        if [ -n "$_current" ] && [ "$_current" = "$VERSION" ]; then
            info ""
            info "rpi $_current already installed; reinstalling"
        elif [ -n "$_current" ]; then
            info ""
            info "Upgrading rpi $_current -> $VERSION (stable)"
        fi
    fi

    _installed_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    _manifest_target=$TARGET
    [ "$IS_STATIC" = 1 ] && _manifest_target=$TARGET-static
    {
        printf '{\n'
        printf '  "version": "%s",\n' "$(json_escape "$VERSION")"
        printf '  "target": "%s",\n' "$(json_escape "$_manifest_target")"
        printf '  "installedAt": "%s",\n' "$(json_escape "$_installed_at")"
        printf '  "sourceUrl": "%s",\n' "$(json_escape "$ARCHIVE_URL")"
        printf '  "sha256": "%s",\n' "$(json_escape "$EXPECTED_SHA")"
        printf '  "installPath": "%s",\n' "$(json_escape "$INSTALL_PATH")"
        printf '  "method": "binary"\n'
        printf '}\n'
    } >"$_stage_manifest" || die "failed to write the staged manifest"

    # Both files are staged in the target directory; the two renames are the
    # only mutations of an existing install (a fatal failure earlier leaves
    # the old binary and manifest untouched).
    mv -f "$_stage_bin" "$INSTALL_PATH" || die "failed to replace $INSTALL_PATH"
    STAGE_BIN=""
    mv -f "$_stage_manifest" "$MANIFEST" || die "failed to replace $MANIFEST"
    STAGE_MANIFEST=""
}

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

sudo_hint() {
    die "install directory $PREFIX is not writable — re-run with sudo, e.g.:
  curl -fsSL https://revpi.dev/install.sh | sudo sh -s -- --prefix \"$PREFIX\""
}

# ---- plugin materialization -------------------------------------------------

json_string_field() { # json_string_field <file> <key>
    sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" | head -n 1
}

json_number_field() { # json_number_field <file> <key>
    sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1" | head -n 1
}

# Semver <= comparison (rpi-docs/install-packaging.md §6.4):
#   0 = true, 1 = false, 2 = invalid input.
semver_core_cmp() { # -> 1 (a<b), 0 (equal), 2 (a>b), 3 (invalid)
    _a_core=$1
    _b_core=$2
    _old_ifs=$IFS
    IFS=.
    set -- $_a_core $_b_core
    IFS=$_old_ifs
    [ $# -eq 6 ] || return 3
    _a1=$1 _a2=$2 _a3=$3 _b1=$4 _b2=$5 _b3=$6
    for _n in "$_a1" "$_a2" "$_a3" "$_b1" "$_b2" "$_b3"; do
        case $_n in
            '' | *[!0-9]*) return 3 ;;
        esac
        [ "$_n" = 0 ] || [ "${_n#0}" = "$_n" ] || return 3
    done
    if [ "$_a1" -lt "$_b1" ]; then return 1; fi
    if [ "$_a1" -gt "$_b1" ]; then return 2; fi
    if [ "$_a2" -lt "$_b2" ]; then return 1; fi
    if [ "$_a2" -gt "$_b2" ]; then return 2; fi
    if [ "$_a3" -lt "$_b3" ]; then return 1; fi
    if [ "$_a3" -gt "$_b3" ]; then return 2; fi
    return 0
}

string_cmp() { # -> 1 (a<b), 0 (equal), 2 (a>b)
    if expr "x$a" \< "x$b" >/dev/null 2>&1; then return 1; fi
    if expr "x$a" \> "x$b" >/dev/null 2>&1; then return 2; fi
    return 0
}

semver_pre_cmp() { # -> 1 (a<b), 0 (equal), 2 (a>b), 3 (invalid)
    _a_rest=$1
    _b_rest=$2
    while [ -n "$_a_rest" ] || [ -n "$_b_rest" ]; do
        _a_id=${_a_rest%%.*}
        _b_id=${_b_rest%%.*}
        if [ "$_a_rest" = "$_a_id" ]; then _a_rest=""; else _a_rest=${_a_rest#*.}; fi
        if [ "$_b_rest" = "$_b_id" ]; then _b_rest=""; else _b_rest=${_b_rest#*.}; fi
        if [ -z "$_a_id" ]; then return 1; fi
        if [ -z "$_b_id" ]; then return 2; fi
        _a_num=0
        _b_num=0
        case $_a_id in *[!0-9]*) ;; *) _a_num=1 ;; esac
        case $_b_id in *[!0-9]*) ;; *) _b_num=1 ;; esac
        if [ "$_a_num" = 1 ] && [ "$_b_num" = 1 ]; then
            case $_a_id in
                0) ;;
                *) [ "${_a_id#0}" = "$_a_id" ] || return 3 ;;
            esac
            case $_b_id in
                0) ;;
                *) [ "${_b_id#0}" = "$_b_id" ] || return 3 ;;
            esac
            if [ "$_a_id" -lt "$_b_id" ]; then return 1; fi
            if [ "$_a_id" -gt "$_b_id" ]; then return 2; fi
        elif [ "$_a_num" = 1 ]; then
            return 1
        elif [ "$_b_num" = 1 ]; then
            return 2
        else
            string_cmp "$_a_id" "$_b_id"
            _r=$?
            [ "$_r" = 0 ] || return "$_r"
        fi
    done
    return 0
}

semver_pre_valid() { # semver_pre_valid <prerelease> -> 0 valid
    _pre=$1
    [ -n "$_pre" ] || return 1
    _old_ifs=$IFS
    IFS=.
    set -- $_pre
    IFS=$_old_ifs
    for _id in "$@"; do
        [ -n "$_id" ] || return 1
        case $_id in
            *[!0-9A-Za-z-]*) return 1 ;;
        esac
        case $_id in
            *[!0-9]*) ;; # alphanumeric identifier: anything goes
            *)
                [ "$_id" = 0 ] || [ "${_id#0}" = "$_id" ] || return 1
                ;;
        esac
    done
    return 0
}

semver_le() { # semver_le <a> <b> (build metadata ignored) -> 0 true, 1 false, 2 invalid
    a=$1
    b=$2
    case $a in '' | *[!0-9A-Za-z.+-]*) return 2 ;; esac
    case $b in '' | *[!0-9A-Za-z.+-]*) return 2 ;; esac

    # Build metadata must be a non-empty dot-separated identifier list and is
    # ignored for ordering.
    case $a in
        *+*)
            _a_build=${a#*+}
            case $_a_build in
                '' | *[!0-9A-Za-z.-]*) return 2 ;;
            esac
            a=${a%%+*}
            ;;
    esac
    case $b in
        *+*)
            _b_build=${b#*+}
            case $_b_build in
                '' | *[!0-9A-Za-z.-]*) return 2 ;;
            esac
            b=${b%%+*}
            ;;
    esac
    case $a in *+*) return 2 ;; esac
    case $b in *+*) return 2 ;; esac

    case $a in *-) return 2 ;; esac
    case $b in *-) return 2 ;; esac
    a_pre=""
    b_pre=""
    case $a in
        *-*)
            a_pre=${a#*-}
            a=${a%%-*}
            ;;
    esac
    case $b in
        *-*)
            b_pre=${b#*-}
            b=${b%%-*}
            ;;
    esac
    if [ -n "$a_pre" ] && ! semver_pre_valid "$a_pre"; then return 2; fi
    if [ -n "$b_pre" ] && ! semver_pre_valid "$b_pre"; then return 2; fi

    semver_core_cmp "$a" "$b"
    _core=$?
    case $_core in
        1) return 0 ;;
        2) return 1 ;;
        3) return 2 ;;
    esac
    if [ -z "$a_pre" ] && [ -z "$b_pre" ]; then return 0; fi
    if [ -z "$a_pre" ]; then return 1; fi
    if [ -z "$b_pre" ]; then return 0; fi
    semver_pre_cmp "$a_pre" "$b_pre"
    _pre=$?
    case $_pre in
        1) return 0 ;;
        2) return 1 ;;
        3) return 2 ;;
        *) return 0 ;;
    esac
}

safe_rpix_members() { # safe_rpix_members <rpix>
    tar -tzf "$1" >"$TMPDIR_RPI/rpix-members" 2>/dev/null ||
        return 1
    while IFS= read -r _name; do
        case $_name in
            '' | /* | .. | ../* | */.. | */../*)
                return 1
                ;;
        esac
    done <"$TMPDIR_RPI/rpix-members"
    tar -tvzf "$1" 2>/dev/null | cut -c1 | sort -u >"$TMPDIR_RPI/rpix-types"
    while IFS= read -r _type; do
        case $_type in
            - | d) ;;
            *) return 1 ;;
        esac
    done <"$TMPDIR_RPI/rpix-types"
    return 0
}

verify_rpix_sums() { # verify_rpix_sums <dir>
    _dir=$1
    _sums=$_dir/SHA256SUMS
    [ -f "$_sums" ] || return 1
    : >"$TMPDIR_RPI/rpix-listed"
    _line_no=0
    while IFS= read -r _line; do
        _line_no=$((_line_no + 1))
        [ -n "$_line" ] || continue
        _hex=$(printf '%s' "$_line" | sed -n 's/^\([0-9a-fA-F]\{64\}\)[[:space:]]\+.*$/\1/p')
        _name=$(printf '%s' "$_line" | sed -n 's/^[0-9a-fA-F]\{64\}[[:space:]]\+//p')
        [ -n "$_hex" ] && [ -n "$_name" ] || {
            warn "malformed SHA256SUMS line $_line_no"
            return 1
        }
        case $_name in
            '' | /* | .. | ../* | */.. | */../*) return 1 ;;
        esac
        _file=$_dir/$_name
        [ -f "$_file" ] || {
            warn "SHA256SUMS lists a missing file: $_name"
            return 1
        }
        [ "$(sha256_file "$_file")" = "$(printf '%s' "$_hex" | tr '[:upper:]' '[:lower:]')" ] || {
            warn "SHA256SUMS mismatch: $_name"
            return 1
        }
        case " $(cat "$TMPDIR_RPI/rpix-listed") " in
            *" $_name "*)
                warn "SHA256SUMS lists $_name more than once"
                return 1
                ;;
        esac
        printf '%s\n' "$_name" >>"$TMPDIR_RPI/rpix-listed"
    done <"$_sums"

    # Every extracted regular file (except SHA256SUMS) must be listed once,
    # including files nested in subdirectories.
    find "$_dir" -type f >"$TMPDIR_RPI/rpix-files"
    while IFS= read -r _f; do
        _rel=${_f#"$_dir"/}
        [ "$_rel" = SHA256SUMS ] && continue
        grep -Fxq "$_rel" "$TMPDIR_RPI/rpix-listed" || {
            warn "file not covered by SHA256SUMS: $_rel"
            return 1
        }
    done <"$TMPDIR_RPI/rpix-files"
    return 0
}

materialize_plugin() { # materialize_plugin <plugin> <bundle-dir> -> 0 ok
    _plugin=$1
    _bundle=$2
    _rpix_name=$_plugin-$VERSION-$TARGET.rpix
    _rpix=$_bundle/extensions/$_rpix_name
    _sidecar=$_rpix.sha256
    [ -f "$_rpix" ] || { warn "$_plugin: missing $_rpix_name"; return 1; }
    [ -f "$_sidecar" ] || { warn "$_plugin: missing $_rpix_name.sha256"; return 1; }

    _sidecar_sha=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$_sidecar" | head -n 1)
    [ -n "$_sidecar_sha" ] || { warn "$_plugin: malformed sidecar"; return 1; }
    [ "$(sha256_file "$_rpix")" = "$(printf '%s' "$_sidecar_sha" | tr '[:upper:]' '[:lower:]')" ] || {
        warn "$_plugin: .rpix sidecar mismatch"
        return 1
    }

    safe_rpix_members "$_rpix" || {
        warn "$_plugin: unsafe or unsupported .rpix members"
        return 1
    }
    _tmp=$PLUGIN_DIR/$_plugin.tmp-$$
    _old=$PLUGIN_DIR/$_plugin.old-$$
    rm -rf "$_tmp" "$_old"
    mkdir -p "$_tmp" || { warn "$_plugin: cannot create $_tmp"; return 1; }
    if ! tar -xzf "$_rpix" -C "$_tmp"; then
        warn "$_plugin: extraction failed"
        rm -rf "$_tmp"
        return 1
    fi
    if ! verify_rpix_sums "$_tmp"; then
        rm -rf "$_tmp"
        return 1
    fi

    _manifest=$_tmp/rpi-extension.json
    [ -f "$_manifest" ] || { warn "$_plugin: missing rpi-extension.json"; rm -rf "$_tmp"; return 1; }
    _name=$(json_string_field "$_manifest" name)
    _version=$(json_string_field "$_manifest" version)
    _abi=$(json_number_field "$_manifest" rpiAbi)
    _min_host=$(json_string_field "$_manifest" minHostVersion)
    [ "$_name" = "$_plugin" ] || { warn "$_plugin: manifest name mismatch ($_name)"; rm -rf "$_tmp"; return 1; }
    [ "$_version" = "$VERSION" ] || { warn "$_plugin: manifest version mismatch ($_version)"; rm -rf "$_tmp"; return 1; }
    [ "$_abi" = 1 ] || { warn "$_plugin: unsupported rpiAbi ($_abi)"; rm -rf "$_tmp"; return 1; }
    if [ -n "$_min_host" ]; then
        if semver_le "$_min_host" "$VERSION"; then
            :
        else
            _r=$?
            rm -rf "$_tmp"
            if [ "$_r" = 2 ]; then
                warn "$_plugin: invalid minHostVersion ($_min_host)"
            else
                warn "$_plugin: requires host >= $_min_host, bundle is $VERSION"
            fi
            return 1
        fi
    fi

    if [ -d "$PLUGIN_DIR/$_plugin" ]; then
        mv "$PLUGIN_DIR/$_plugin" "$_old" || { warn "$_plugin: cannot move the old directory"; rm -rf "$_tmp"; return 1; }
    fi
    if ! mv "$_tmp" "$PLUGIN_DIR/$_plugin"; then
        warn "$_plugin: atomic replace failed"
        [ -d "$_old" ] && mv "$_old" "$PLUGIN_DIR/$_plugin"
        rm -rf "$_tmp"
        return 1
    fi
    rm -rf "$_old"
    return 0
}

materialize_plugins() { # materialize_plugins <bundle-dir>
    _bundle=$1
    PLUGINS_TOTAL=0
    for _plugin in $(plugins_for_profile "$PROFILE"); do
        PLUGINS_TOTAL=$((PLUGINS_TOTAL + 1))
        if materialize_plugin "$_plugin" "$_bundle"; then
            PLUGINS_INSTALLED=$((PLUGINS_INSTALLED + 1))
        else
            PLUGINS_FAILED=$((PLUGINS_FAILED + 1))
            FAILED_PLUGIN_LINES="$FAILED_PLUGIN_LINES  retry: rpi install $_plugin
"
        fi
    done
}

seed_settings() { # seed_settings <bundle-dir>
    _bundle=$1
    if [ "$PROFILE" = mini ] || [ "$IS_STATIC" = 1 ]; then
        return 0
    fi
    if [ "$PLUGINS_FAILED" != 0 ] || [ "$PLUGINS_INSTALLED" != "$PLUGINS_TOTAL" ]; then
        info ""
        warn "skipping settings seeding because not all plugins were installed"
        return 0
    fi
    if [ -f "$SETTINGS_FILE" ]; then
        info ""
        info "existing settings left untouched: $SETTINGS_FILE"
        if ! grep -q '"packages"' "$SETTINGS_FILE"; then
            info "note: no 'packages' entry found; installed plugins will show as (untracked)."
            info "      To track them, run: rpi install rpi-statusline (and other plugins)"
        fi
        return 0
    fi
    _defaults=$_bundle/settings.defaults.json
    if [ ! -f "$_defaults" ]; then
        warn "bundle has no settings.defaults.json; skipping settings seeding"
        return 0
    fi
    mkdir -p "$AGENT_DIR" 2>/dev/null || { warn "cannot create $AGENT_DIR; skipping settings seeding"; return 0; }
    chmod 700 "$AGENT_DIR" 2>/dev/null || true
    _tmp=$SETTINGS_FILE.tmp-$$
    cp "$_defaults" "$_tmp" || { warn "cannot stage settings.json"; rm -f "$_tmp"; return 0; }
    chmod 600 "$_tmp" 2>/dev/null || true
    mv -f "$_tmp" "$SETTINGS_FILE" || { warn "cannot install settings.json"; rm -f "$_tmp"; return 0; }
    info "seeded default settings: $SETTINGS_FILE"
}

# ---- PATH integration -------------------------------------------------------

is_rpi_active_on_path() {
    active=$(command -v rpi 2>/dev/null || true)
    [ -n "$active" ] && [ "$active" = "$INSTALL_PATH" ]
}

is_user_scoped_install() {
    [ -n "${HOME_DIR:-}" ] || return 1
    [ "$(id -u 2>/dev/null || printf '1')" != 0 ] || return 1
    case "$PREFIX_ABS" in
        "$HOME_DIR"/*) return 0 ;;
    esac
    return 1
}

shell_config_file() {
    [ -n "${HOME_DIR:-}" ] || return 1
    shell_name=$(basename "${SHELL:-sh}")
    case $shell_name in
        fish)
            config_home=${XDG_CONFIG_HOME:-$HOME_DIR/.config}
            config_home=$(normalize_dir "$config_home")
            printf '%s' "$config_home/fish/config.fish"
            ;;
        zsh)
            zsh_home=${ZDOTDIR:-$HOME_DIR}
            zsh_home=$(normalize_dir "$zsh_home")
            printf '%s' "$zsh_home/.zshrc"
            ;;
        bash)
            if [ -f "$HOME_DIR/.bashrc" ]; then
                printf '%s' "$HOME_DIR/.bashrc"
            elif [ -f "$HOME_DIR/.bash_profile" ]; then
                printf '%s' "$HOME_DIR/.bash_profile"
            elif [ -f "$HOME_DIR/.bash_login" ]; then
                printf '%s' "$HOME_DIR/.bash_login"
            else
                printf '%s' "$HOME_DIR/.profile"
            fi
            ;;
        sh | dash | ash | ksh | mksh | pdksh | posh | yash)
            printf '%s' "$HOME_DIR/.profile"
            ;;
        *)
            return 1
            ;;
    esac
}

# Escape a custom path for use inside double quotes in a shell profile.
# The default ~/.local/bin is emitted as $HOME/.local/bin instead, so the
# profile stays portable across machines.
profile_double_quote() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\$/\\$/g; s/`/\\`/g'
}

path_update_command() {
    bin_dir=$1
    shell_name=$(basename "${SHELL:-sh}")
    if [ -n "${HOME_DIR:-}" ] && [ "$bin_dir" = "$HOME_DIR/.local/bin" ]; then
        bin_expr='$HOME/.local/bin'
    else
        bin_expr=$(profile_double_quote "$bin_dir")
    fi
    case $shell_name in
        fish)
            printf 'fish_add_path "%s"' "$bin_expr"
            ;;
        sh | dash | ash | ksh | mksh | pdksh | posh | yash | bash | zsh)
            # Keep the whole guard on one line so the installer block stays a
            # two-line unit; the guard prevents nested shells from appending
            # the same entry on every source.
            printf 'case ":$PATH:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac' "$bin_expr" "$bin_expr"
            ;;
        *)
            return 1
            ;;
    esac
}

config_file_mentions_path() {
    file=$1
    command=$2
    dir=$3
    [ -f "$file" ] || return 1
    grep -Fxq "$command" "$file" && return 0
    legacy_abs=$(printf 'export PATH="%s:$PATH"' "$(profile_double_quote "$dir")")
    if [ "$legacy_abs" != "$command" ] && grep -Fxq "$legacy_abs" "$file"; then
        return 0
    fi
    if [ -n "${HOME_DIR:-}" ] && [ "$dir" = "$HOME_DIR/.local/bin" ]; then
        grep -Fxq 'export PATH="$HOME/.local/bin:$PATH"' "$file" && return 0
    fi
    return 1
}

# Remove a previous install.sh-written block: a `# rpi` marker followed by an
# export/fish_add_path/case line. Unrelated lines are preserved. The file is
# rewritten in place (cat >), which keeps symlinks, permissions and ownership
# intact; `mv` would replace a dotfiles symlink with a regular file.
remove_rpi_path_block() {
    config_file=$1
    tmp="${config_file}.rpi-tmp.$$"
    if ! awk '
        $0 == "# rpi" {
            skip = 1
            if (out_count > 0 && out[out_count] == "") {
                out_count--
            }
            next
        }
        skip == 1 {
            if ($0 ~ /^export PATH=/ || $0 ~ /^fish_add_path / || $0 ~ /^case ":\$PATH:" in /) {
                skip = 0
                next
            }
            skip = 0
        }
        { out[++out_count] = $0 }
        END { for (i = 1; i <= out_count; i++) print out[i] }
    ' "$config_file" >"$tmp"; then
        rm -f "$tmp"
        return 1
    fi
    if ! cat "$tmp" >"$config_file"; then
        rm -f "$tmp"
        return 1
    fi
    rm -f "$tmp"
    return 0
}

append_path_to_profile() {
    config_file=$1
    command=$2
    mkdir -p "${config_file%/*}" 2>/dev/null || return 1
    [ -e "$config_file" ] || : >"$config_file" || return 1
    if grep -Fxq '# rpi' "$config_file" 2>/dev/null; then
        remove_rpi_path_block "$config_file" || return 1
    fi
    if [ -s "$config_file" ]; then
        last_byte=$(tail -c 1 "$config_file" 2>/dev/null || true)
        if [ -n "$last_byte" ]; then
            printf '\n' >>"$config_file" || return 1
        fi
    fi
    printf '# rpi\n%s\n' "$command" >>"$config_file" || return 1
    return 0
}

can_prompt_for_path() {
    [ -t 1 ] || return 1
    [ -z "${CI:-}" ] || return 1
    is_truthy "${RPI_NONINTERACTIVE:-}" && return 1
    case ${TERM:-} in
        dumb) return 1 ;;
    esac
    ( : <>/dev/tty ) 2>/dev/null || return 1
    return 0
}

prompt_add_path_to_profile() {
    bin_dir=$1
    if ! config_file=$(shell_config_file); then
        return 1
    fi
    if ! command=$(path_update_command "$bin_dir"); then
        return 1
    fi

    if config_file_mentions_path "$config_file" "$command" "$bin_dir"; then
        info "A PATH update for $bin_dir already exists in $config_file."
        return 0
    fi

    if ! can_prompt_for_path; then
        return 1
    fi

    # curl | sh means stdin is the installer script itself; ask on /dev/tty.
    exec 3<>/dev/tty || return 1
    if ! printf 'Add %s to your PATH in %s now? [Y/n] ' "$bin_dir" "$config_file" >&3; then
        exec 3>&-
        return 1
    fi
    if [ "$READ_TIMEOUT_SUPPORTED" = 1 ]; then
        if ! IFS= read -r -t "$PROMPT_TIMEOUT" answer <&3; then
            answer=N
        fi
    else
        if ! IFS= read -r answer <&3; then
            answer=N
        fi
    fi
    exec 3>&-
    answer=$(printf '%s' "$answer" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')
    case $answer in
        n | no) return 1 ;;
    esac

    if append_path_to_profile "$config_file" "$command"; then
        info "Added $bin_dir to $config_file."
        return 0
    fi
    warn "could not update $config_file"
    return 1
}

print_path_guidance() {
    info ""
    info "note: $PREFIX_ABS is not the active rpi on PATH; add it with:"
    if command=$(path_update_command "$PREFIX_ABS"); then
        info "  $command"
        if [ -n "${HOME_DIR:-}" ]; then
            if config_file=$(shell_config_file); then
                info "(append that line to $config_file to make it permanent)"
            fi
        fi
    else
        info "  add $PREFIX_ABS to PATH in your shell configuration manually."
        info "  (automatic profile update is not supported for shell $(basename "${SHELL:-sh}"))"
    fi
}

maybe_update_path() {
    if is_rpi_active_on_path; then
        info ""
        info "rpi is already on PATH."
        return 0
    fi

    if ! is_user_scoped_install; then
        print_path_guidance
        info "note: not modifying a shell profile for a root, system-wide, or outside-\$HOME install."
        return 0
    fi

    case $PATH_ACTION in
        add)
            if ! config_file=$(shell_config_file); then
                print_path_guidance
                return 0
            fi
            if ! command=$(path_update_command "$PREFIX_ABS"); then
                print_path_guidance
                return 0
            fi
            if config_file_mentions_path "$config_file" "$command" "$PREFIX_ABS"; then
                info ""
                info "A PATH update for $PREFIX_ABS already exists in $config_file."
            elif append_path_to_profile "$config_file" "$command"; then
                info ""
                info "Added $PREFIX_ABS to $config_file."
            else
                print_path_guidance
                return 0
            fi
            info "Restart your shell or run:"
            info "  $command"
            ;;
        skip)
            print_path_guidance
            ;;
        *)
            if prompt_add_path_to_profile "$PREFIX_ABS"; then
                info "Restart your shell or run:"
                info "  $(path_update_command "$PREFIX_ABS")"
            else
                print_path_guidance
            fi
            ;;
    esac
}

# ---- main -------------------------------------------------------------------

# Test seam: `RPI_INSTALL_TEST_SOURCE=1 . install.sh` defines the helpers
# (semver parsing etc.) without running an install.
if [ "${RPI_INSTALL_TEST_SOURCE:-}" = 1 ]; then
    return 0 2>/dev/null || exit 0
fi


detect_target
TMPDIR_RPI=$(mktemp -d 2>/dev/null || mktemp -d -t rpi-install)
info "detected platform: $TARGET"
if [ "$IS_MUSL_TARGET" = 1 ]; then
    info "  musl target: the main binary is dynamically linked (libgcc; standard/full also need libstdc++)."
fi

resolve_version
info "latest release: v$VERSION (via $RESOLVED_VIA)"

select_profile

choose_static_fallback
if [ "$IS_STATIC" = 1 ]; then
    PROFILE_EFFECTIVE=mini
else
    PROFILE_EFFECTIVE=$PROFILE
fi

# Only standard/full/static need static-fallback reasoning; a dynamic mini
# still needs libgcc on musl (already covered by choose_static_fallback).
if [ "$IS_STATIC" != 1 ] && [ "$IS_MUSL_TARGET" = 1 ]; then
    # choose_static_fallback already verified the dynamic runtime for the
    # selected profile; nothing else to do here.
    :
fi

build_asset_name
if [ "$IS_STATIC" != 1 ] && [ "$PROFILE" != mini ]; then
    read_profile_index "$VERSION" "$ASSET_NAME"
fi

CANDIDATES=""
build_candidates

resolve_cache_dir

# Sidecar first: it gives us the expected archive hash before the big transfer.
info ""
info "fetching $ASSET_NAME.sha256"
SIDECAR_CANDIDATES=""
_ifs=$IFS
IFS='
'
for _u in $CANDIDATES; do
    IFS=$_ifs
    SIDECAR_CANDIDATES="$SIDECAR_CANDIDATES$_u.sha256
"
    IFS='
'
done
IFS=$_ifs
download_asset "$ASSET_NAME.sha256" "$SIDECAR_CANDIDATES" ""
EXPECTED_SHA=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$CACHE_PARTS/$ASSET_NAME.sha256" | head -n 1)
EXPECTED_SHA=$(printf '%s' "$EXPECTED_SHA" | tr '[:upper:]' '[:lower:]')
case $EXPECTED_SHA in
    '' | *[!0-9a-f]*) die "malformed checksum in $ASSET_NAME.sha256" ;;
esac
if [ -n "$INDEX_SHA" ] && [ "$EXPECTED_SHA" != "$INDEX_SHA" ]; then
    rm -f "$CACHE_PARTS/$ASSET_NAME.sha256"
    die "Release sidecar does not match the rpi-pages profile index for $ASSET_NAME (sidecar=$EXPECTED_SHA, index=$INDEX_SHA); aborting"
fi

info ""
download_asset "$ASSET_NAME" "$CANDIDATES" "$EXPECTED_SHA"
if [ "$DOWNLOADED_FROM" = cache ]; then
    ARCHIVE_URL=$(meta_value "$CACHE_PARTS/$ASSET_NAME.meta" url)
    [ -n "$ARCHIVE_URL" ] || ARCHIVE_URL=$RELEASE_BASE_URL/download/v$VERSION/$ASSET_NAME
else
    ARCHIVE_URL=$DOWNLOADED_FROM
fi
verify_outer_integrity
validate_outer_archive "$CACHE_PARTS/$ASSET_NAME"
extract_outer_archive "$CACHE_PARTS/$ASSET_NAME" "$TMPDIR_RPI/x"

# The agent dir is only needed for plugin materialization and settings
# seeding; mini/static installs work without HOME.
AGENT_DIR=${RPI_CODING_AGENT_DIR:-}
PLUGIN_DIR=""
SETTINGS_FILE=""
if [ "$PROFILE_EFFECTIVE" != mini ]; then
    if [ -z "$AGENT_DIR" ]; then
        [ -n "${HOME:-}" ] || die "\$HOME is not set and RPI_CODING_AGENT_DIR is unset; cannot locate the agent directory"
        AGENT_DIR=$HOME/.rpi/agent
    fi
    PLUGIN_DIR=$AGENT_DIR/extensions
    SETTINGS_FILE=$AGENT_DIR/settings.json
fi

_binary_name=rpi
if [ "$PROFILE_EFFECTIVE" != mini ]; then
    mkdir -p "$PLUGIN_DIR" || die "cannot create $PLUGIN_DIR"
fi
install_binary "$TMPDIR_RPI/x" "$_binary_name"

if [ "$PROFILE_EFFECTIVE" != mini ]; then
    materialize_plugins "$TMPDIR_RPI/x"
    seed_settings "$TMPDIR_RPI/x"
fi

# ---- summary ----------------------------------------------------------------

info ""
if [ "$IS_STATIC" = 1 ]; then
    info "rpi $VERSION installed (static fallback) · profile mini"
else
    info "rpi $VERSION installed · stable · profile $PROFILE"
fi
info "  binary:   $INSTALL_PATH"
info "  asset:    $ASSET_NAME"
info "  manifest: $MANIFEST"
if [ "$IS_STATIC" = 1 ]; then
    info "  note:     this build cannot load native plugins."
    info "            To enable plugins, install libgcc + libstdc++ and re-run:"
    info "              apk add libgcc libstdc++ && curl -fsSL https://revpi.dev/install.sh | sh"
elif [ "$PROFILE" != mini ]; then
    _names=""
    for _p in $(plugins_for_profile "$PROFILE"); do
        _names="$_names$_p $VERSION · "
    done
    info "  plugins:  ${_names% · }"
    info "            $PLUGINS_INSTALLED/$PLUGINS_TOTAL installed"
    if [ -f "$SETTINGS_FILE" ]; then
        info "            registered in $SETTINGS_FILE"
    fi
fi

if [ "$PLUGINS_FAILED" != 0 ]; then
    warn "$PLUGINS_FAILED of $PLUGINS_TOTAL plugins failed"
    printf '%s' "$FAILED_PLUGIN_LINES" >&2
    warn "the rpi binary was installed; re-run the installer or use the retry commands above"
    if is_truthy "${RPI_INSTALL_REQUIRE_PLUGINS:-}"; then
        exit 1
    fi
fi

info ""
info "next steps:"
if [ "$IS_STATIC" != 1 ] && [ "$PROFILE" != mini ]; then
    info "  rpi                  start rpi"
    info "  rpi update --all     update rpi + plugins   (self: rpi update --self)"
    info "  rpi list             see installed plugins"
    info "  rpi remove <name>    remove one plugin (e.g. rpi remove rpi-todo)"
    info "  rpi self-uninstall   remove rpi (--purge deletes ~/.rpi)"
else
    info "  update:    rpi update --self"
    info "  uninstall: rpi self-uninstall   (add --purge to also delete ~/.rpi)"
fi
info "docs: https://revpi.dev/docs.html"

maybe_update_path