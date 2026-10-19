#!/bin/sh
# install.sh — official installer for the rpi CLI (POSIX sh; no bash required).
#
# Usage:
#   curl -fsSL https://revpi.dev/install.sh | sh
#   sh install.sh --prefix /usr/local/bin
#   sh install.sh --add-path
#   sh install.sh --musl
#
# Options:
#   --prefix <dir>   Install directory (default: ~/.local/bin)
#   --add-path       Add the install directory to the detected shell profile
#                    without prompting (user-scoped installs and supported
#                    shells only; also honors RPI_INSTALL_ADD_PATH=1)
#   --no-add-path    Never modify a shell profile (also honors
#                    RPI_INSTALL_ADD_PATH=0)
#   --musl           Force the musl target on Linux (auto-detected via ldd otherwise)
#   -h, --help       Show usage
#
# PATH behavior: when the install directory is not already the active `rpi`
# on PATH, an interactive run asks once (default: yes) before writing a marked
# `# rpi` block to the detected shell profile (fish/zsh/bash/POSIX profile).
# The prompt requires stdout to be a terminal, a usable /dev/tty, and no
# CI/RPI_NONINTERACTIVE marker; it is skipped when TERM=dumb or stdout is
# redirected. If the shell supports `read -t`, an unanswered prompt times out
# (default 60s) and leaves the profile unchanged. --add-path (or
# RPI_INSTALL_ADD_PATH=1) writes without prompting. --no-add-path, root
# installs, installs outside $HOME, and unsupported shells never modify a
# profile; they print manual guidance (the exact command when the shell is
# supported). CI/pty automation should set RPI_INSTALL_ADD_PATH=0 explicitly.
#
# Environment overrides (for mirrors and tests):
#   RPI_INSTALL_ADD_PATH   1/true/yes = add without prompting;
#                          0/false/no = never add; unset = prompt when possible
#   RPI_NONINTERACTIVE     1/true/yes = never prompt (explicit --add-path or
#                          RPI_INSTALL_ADD_PATH=1 still writes)
#   RPI_INSTALL_PROMPT_TIMEOUT  Seconds before an unanswered prompt is treated
#                          as "no" (default 60; shells without read -t ignore)
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
PATH_ACTION=prompt
ADD_PATH_FLAG=0
NO_ADD_PATH_FLAG=0
TMPDIR_RPI=""
READ_TIMEOUT_SUPPORTED=0
PROMPT_TIMEOUT=${RPI_INSTALL_PROMPT_TIMEOUT:-60}

info() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
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
Usage: install.sh [--prefix <dir>] [--add-path|--no-add-path] [--musl]

Options:
  --prefix <dir>   Install directory (default: ~/.local/bin)
  --add-path       Add the install directory to the detected shell profile
                   without prompting (user-scoped installs and supported
                   shells only)
  --no-add-path    Never modify a shell profile
  --musl           Force the musl target on Linux (auto-detected otherwise)
  -h, --help       Show this help

Environment overrides (mirrors/tests): RPI_INSTALL_ADD_PATH=1|0,
RPI_NONINTERACTIVE=1, RPI_INSTALL_PROMPT_TIMEOUT=<seconds>,
RPI_GITHUB_API_URL, RPI_VERSION_CHECK_URL, RPI_RELEASE_BASE_URL,
RPI_SITE_BASE_URL — see the script header.

CI/pty automation should set RPI_INSTALL_ADD_PATH=0 so an install never waits
for terminal input.
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
    add_path_env=$(printf '%s' "${RPI_INSTALL_ADD_PATH:-}" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    case $add_path_env in
        1 | true | yes | on) PATH_ACTION=add ;;
        0 | false | no | off) PATH_ACTION=skip ;;
    esac
fi

# Feature-probe `read -t` once: bash/ksh/zsh support it, dash/POSIX sh does
# not. The prompt falls back to a normal blocking read on shells without it.
if (IFS= read -r -t 0 _ <<EOF
x
EOF
) 2>/dev/null; then
    READ_TIMEOUT_SUPPORTED=1
fi
case $PROMPT_TIMEOUT in
    '' | *[!0-9]*) PROMPT_TIMEOUT=60 ;;
esac

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
    # Recognize the simple export form written by older installers/manual
    # guidance (absolute path) and by pre-guard versions of this installer
    # ($HOME form for the default prefix).
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
            # Drop the separating blank line together with the block so
            # repeated prefix changes do not accumulate blank lines.
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

# Prompting is only safe when a human can answer. CI/pty runners often expose
# /dev/tty without ever writing to it, so a plain readiness check is not
# enough; honor the standard CI marker and RPI_NONINTERACTIVE as well.
can_prompt_for_path() {
    [ -t 1 ] || return 1
    [ -z "${CI:-}" ] || return 1
    noninteractive=$(printf '%s' "${RPI_NONINTERACTIVE:-}" | tr '[:upper:]' '[:lower:]' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    case $noninteractive in
        1 | true | yes | on) return 1 ;;
    esac
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

# ---- guidance ---------------------------------------------------------------

info ""
info "rpi $VERSION installed to $INSTALL_PATH"
info "  target:   $TARGET"
info "  manifest: $MANIFEST"
info ""
info "next steps:"
info "  update:    rpi update --self"
info "  uninstall: rpi self-uninstall   (add --purge to also delete ~/.rpi)"

maybe_update_path
