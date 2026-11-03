#!/bin/sh
# Fork releases are self-contained: resolve latest once, then verify and install
# assets from that immutable tag so publication cannot mix release generations.
#
# Two steps, in order:
#   1. herdr:       herdr-<os>-<arch>                -> $HERDR_INSTALL_DIR/herdr
#   2. herdr-tools: herdr-tools-<os>-<arch>.tar.gz   -> the shadowfax.tools plugin
#      dir under ${XDG_DATA_HOME:-~/.local/share}/herdr/plugins, then linked
#      with the herdr from step 1. HERDR_INSTALL_TOOLS=0 skips this step.
# Each asset is verified against the tag's SHA256SUMS before it replaces
# anything. A failed step 2 keeps the herdr from step 1, exits non-zero and
# prints the exact command that retries it.
set -eu

BIN="herdr"
RELEASES_URL="https://github.com/shadowfax92/herdr/releases"
INSTALL_DIR="${HERDR_INSTALL_DIR:-$HOME/.local/bin}"
TOOLS_PLUGIN_ID="shadowfax.tools"
# Set once herdr is in place; the exit trap then knows a failure belongs to the
# tools step and how to retry it.
HERDR_INSTALLED=""
TOOLS_STAGE=""

main() {
    OS="$(uname -s)"
    case "$OS" in
        Linux) os="linux" ;;
        Darwin) os="macos" ;;
        *) err "unsupported OS: $OS" ;;
    esac
    if [ "$OS" = "Linux" ] && [ "$(uname -o 2>/dev/null || true)" = "Android" ]; then
        err "unsupported OS: Android/Termux"
    fi

    ARCH="$(uname -m)"
    case "$ARCH" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) err "unsupported architecture: $ARCH" ;;
    esac
    ASSET="herdr-${os}-${arch}"
    TOOLS_ASSET="herdr-tools-${os}-${arch}.tar.gz"
    need curl
    need awk

    if command -v sha256sum >/dev/null 2>&1; then
        SHA256_TOOL="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        SHA256_TOOL="shasum"
    elif command -v openssl >/dev/null 2>&1; then
        SHA256_TOOL="openssl"
    else
        err "checksum verification requires sha256sum, shasum, or openssl"
    fi

    log "resolving latest fork release for ${os}/${arch}..."
    RELEASE_URL="$(curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 \
        -o /dev/null -w '%{url_effective}' "${RELEASES_URL}/latest")" \
        || err "cannot resolve the latest fork release at ${RELEASES_URL}"
    case "$RELEASE_URL" in
        "${RELEASES_URL}/tag/fork-v"*) TAG="${RELEASE_URL#"${RELEASES_URL}/tag/"}" ;;
        *) err "latest release is not a fork release: ${RELEASE_URL}" ;;
    esac
    case "$TAG" in
        *[!A-Za-z0-9._-]*) err "invalid fork release tag: ${TAG}" ;;
    esac
    ASSET_URL="${RELEASES_URL}/download/${TAG}"
    TMP="$(mktemp -d)"
    trap cleanup 0
    trap 'exit 1' HUP INT TERM

    curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 \
        "${ASSET_URL}/SHA256SUMS" -o "${TMP}/SHA256SUMS" \
        || err "cannot download release checksums"
    expect_sha256 "$ASSET"

    log "downloading ${TAG}..."
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 \
        "${ASSET_URL}/${ASSET}" -o "${TMP}/${BIN}" \
        || err "binary download failed"
    check_sha256 "${TMP}/${BIN}" "downloaded Herdr checksum did not match"
    log "checksum verified"

    # Never replace an existing installation until the complete download verifies.
    chmod +x "${TMP}/${BIN}"
    mkdir -p "$INSTALL_DIR"
    mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
    HERDR_INSTALLED=1
    log "installed ${TAG} to ${INSTALL_DIR}/${BIN}"

    if [ "${HERDR_INSTALL_TOOLS:-1}" = 0 ]; then
        log "skipping herdr-tools (HERDR_INSTALL_TOOLS=0)"
    else
        install_tools
    fi

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) log "add to your shell config: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
    esac
    log "rerun this installer to update the fork"
}

# Installs the prebuilt shadowfax.tools plugin from the same tag as herdr.
# Every failure goes through err, and the exit trap turns it into a retry hint.
install_tools() {
    need tar
    PLUGINS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/herdr/plugins"
    TOOLS_DIR="${PLUGINS_DIR}/${TOOLS_PLUGIN_ID}"

    expect_sha256 "$TOOLS_ASSET"
    log "downloading ${TOOLS_ASSET}..."
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 \
        "${ASSET_URL}/${TOOLS_ASSET}" -o "${TMP}/${TOOLS_ASSET}" \
        || err "${TOOLS_ASSET} download failed"
    check_sha256 "${TMP}/${TOOLS_ASSET}" "downloaded ${TOOLS_ASSET} checksum did not match"
    log "checksum verified"

    # Unpack beside the destination so the swap below is a same-filesystem
    # rename. Until then the old plugin dir stays exactly as it was.
    mkdir -p "$PLUGINS_DIR"
    TOOLS_STAGE="$(mktemp -d "${PLUGINS_DIR}/.${TOOLS_PLUGIN_ID}.stage.XXXXXX")"
    tar -xzf "${TMP}/${TOOLS_ASSET}" -C "$TOOLS_STAGE" \
        || err "cannot unpack ${TOOLS_ASSET}"
    new="${TOOLS_STAGE}/${TOOLS_PLUGIN_ID}"
    if [ ! -f "${new}/herdr-plugin.toml" ] || [ ! -x "${new}/bin/herdr-tools" ]; then
        err "${TOOLS_ASSET} is not a ${TOOLS_PLUGIN_ID} plugin directory"
    fi

    # rename(2) cannot replace a non-empty directory, so park the old tree in
    # the stage and put it back if the new one cannot move in. The path holds
    # the old tree, then the new one after a brief gap, never a partial
    # unpack. A running herdr-tools keeps its open binary; the next start runs
    # the new one.
    if [ -e "$TOOLS_DIR" ] || [ -L "$TOOLS_DIR" ]; then
        mv "$TOOLS_DIR" "${TOOLS_STAGE}/previous" \
            || err "cannot move the old ${TOOLS_DIR} aside"
    fi
    if ! mv "$new" "$TOOLS_DIR"; then
        if [ -e "${TOOLS_STAGE}/previous" ] || [ -L "${TOOLS_STAGE}/previous" ]; then
            mv "${TOOLS_STAGE}/previous" "$TOOLS_DIR" || true
        fi
        err "cannot move ${TOOLS_PLUGIN_ID} into ${TOOLS_DIR}"
    fi
    rm -rf "$TOOLS_STAGE"
    TOOLS_STAGE=""
    log "installed ${TOOLS_PLUGIN_ID} to ${TOOLS_DIR}"

    # The swap keeps the path, so an existing link already serves the new
    # files. Linking again would re-enable a plugin the user disabled.
    if tools_linked; then
        log "${TOOLS_PLUGIN_ID} is already linked from ${TOOLS_DIR}"
    else
        "${INSTALL_DIR}/${BIN}" plugin link "$TOOLS_DIR" >/dev/null \
            || err "herdr plugin link ${TOOLS_DIR} failed"
        log "linked ${TOOLS_PLUGIN_ID} into herdr"
    fi
}

# True when herdr lists shadowfax.tools with plugin_root equal to TOOLS_DIR.
# Herdr stores the canonical path and prints it as compact JSON, so compare
# against `pwd -P` escaped the way JSON escapes it. Any doubt (an unreadable
# list, a path with control characters) reads as unlinked, so the worst case
# is a redundant link.
tools_linked() {
    tools_root="$(cd "$TOOLS_DIR" && pwd -P)" || return 1
    tools_root_json="$(printf '%s' "$tools_root" | sed 's/[\\"]/\\&/g')"
    plugins="$("${INSTALL_DIR}/${BIN}" plugin list --plugin "$TOOLS_PLUGIN_ID" --json 2>/dev/null)" \
        || return 1
    case "$plugins" in
        *"\"plugin_root\":\"${tools_root_json}\""*) return 0 ;;
    esac
    return 1
}

# Sets EXPECTED_SHA256 to the one SHA256SUMS entry for $1. Rejects missing and
# duplicate entries instead of accepting an ambiguous hash.
expect_sha256() {
    EXPECTED_SHA256="$(awk -v asset="$1" '$2 == asset { count++; hash = $1 }
        END { if (count == 1) print tolower(hash) }' "${TMP}/SHA256SUMS")"
    if [ "${#EXPECTED_SHA256}" -ne 64 ]; then
        err "release has no unique valid checksum for $1"
    fi
    case "$EXPECTED_SHA256" in
        *[!0-9a-f]*) err "release has an invalid checksum for $1" ;;
    esac
}

# Compares file $1 with EXPECTED_SHA256; fails with message $2.
check_sha256() {
    case "$SHA256_TOOL" in
        sha256sum) ACTUAL_SHA256="$(sha256sum < "$1" | awk '{ print $1 }')" ;;
        shasum) ACTUAL_SHA256="$(shasum -a 256 < "$1" | awk '{ print $1 }')" ;;
        openssl) ACTUAL_SHA256="$(openssl dgst -sha256 < "$1" | awk '{ print $NF }')" ;;
    esac
    if [ "$ACTUAL_SHA256" != "$EXPECTED_SHA256" ]; then
        err "$2"
    fi
}

cleanup() {
    status=$?
    rm -rf "$TMP"
    if [ -n "$TOOLS_STAGE" ]; then
        rm -rf "$TOOLS_STAGE"
    fi
    if [ "$status" -ne 0 ] && [ -n "$HERDR_INSTALLED" ]; then
        rerun="sh"
        if [ -n "${HERDR_INSTALL_DIR:-}" ]; then
            rerun="HERDR_INSTALL_DIR=\"${HERDR_INSTALL_DIR}\" sh"
        fi
        printf '  > %s\n' \
            "herdr ${TAG} is installed at ${INSTALL_DIR}/${BIN}, but installing ${TOOLS_PLUGIN_ID} failed." \
            "to retry, rerun:   curl -fsSL ${RELEASES_URL}/latest/download/install.sh | ${rerun}" \
            "or skip the tools: curl -fsSL ${RELEASES_URL}/latest/download/install.sh | HERDR_INSTALL_TOOLS=0 ${rerun}" >&2
    fi
    exit "$status"
}

log() { printf '  > %s\n' "$1"; }
err() { printf 'error: %s\n' "$1" >&2; exit 1; }
need() {
    command -v "$1" >/dev/null 2>&1 || err "requires '$1'"
}

main "$@"
