#!/bin/sh
# Fork releases are self-contained: resolve latest once, then verify and install
# assets from that immutable tag so publication cannot mix release generations.
set -eu

BIN="herdr"
RELEASES_URL="https://github.com/shadowfax92/herdr/releases"
INSTALL_DIR="${HERDR_INSTALL_DIR:-$HOME/.local/bin}"

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
    trap 'rm -rf "$TMP"' 0
    trap 'exit 1' HUP INT TERM

    curl -fsSL --retry 3 --connect-timeout 10 --max-time 30 \
        "${ASSET_URL}/SHA256SUMS" -o "${TMP}/SHA256SUMS" \
        || err "cannot download release checksums"
    # Reject missing and duplicate entries instead of accepting an ambiguous hash.
    SHA256="$(awk -v asset="$ASSET" '$2 == asset { count++; hash = $1 }
        END { if (count == 1) print tolower(hash) }' "${TMP}/SHA256SUMS")"
    if [ "${#SHA256}" -ne 64 ]; then
        err "release has no unique valid checksum for ${ASSET}"
    fi
    case "$SHA256" in
        *[!0-9a-f]*) err "release has an invalid checksum for ${ASSET}" ;;
    esac

    log "downloading ${TAG}..."
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 \
        "${ASSET_URL}/${ASSET}" -o "${TMP}/${BIN}" \
        || err "binary download failed"
    case "$SHA256_TOOL" in
        sha256sum) ACTUAL_SHA256="$(sha256sum < "${TMP}/${BIN}" | awk '{ print $1 }')" ;;
        shasum) ACTUAL_SHA256="$(shasum -a 256 < "${TMP}/${BIN}" | awk '{ print $1 }')" ;;
        openssl) ACTUAL_SHA256="$(openssl dgst -sha256 < "${TMP}/${BIN}" | awk '{ print $NF }')" ;;
    esac
    if [ "$ACTUAL_SHA256" != "$SHA256" ]; then
        err "downloaded Herdr checksum did not match"
    fi
    log "checksum verified"

    # Never replace an existing installation until the complete download verifies.
    chmod +x "${TMP}/${BIN}"
    mkdir -p "$INSTALL_DIR"
    mv "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
    log "installed ${TAG} to ${INSTALL_DIR}/${BIN}"
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) log "add to your shell config: export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
    esac
    log "rerun this installer to update the fork"
}

log() { printf '  > %s\n' "$1"; }
err() { printf 'error: %s\n' "$1" >&2; exit 1; }
need() {
    command -v "$1" >/dev/null 2>&1 || err "requires '$1'"
}

main "$@"
