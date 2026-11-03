#!/bin/sh
# Lays out the shadowfax.tools plugin directory around a prebuilt herdr-tools.
#
#   herdr-tools/scripts/package.sh <herdr-tools-binary> <out-dir>
#
# Writes <out-dir>/shadowfax.tools/{herdr-plugin.toml, bin/herdr-tools, nvim/,
# README.md, LICENSE}. This is the one definition of the plugin layout: the
# fork release tars the result as herdr-tools-<os>-<arch>.tar.gz, which
# distribution/fork-install.sh unpacks, and `just -f herdr-tools/justfile
# install` renames it into place. `herdr plugin link` never builds, so the
# manifest's `./bin/herdr-tools` commands must find the binary right here.
set -eu

usage() {
    printf 'usage: %s <herdr-tools-binary> <out-dir>\n' "$0" >&2
    exit 2
}
err() {
    printf 'package.sh: %s\n' "$1" >&2
    exit 1
}

[ "$#" -eq 2 ] || usage
binary="$1"
out="$2"
# Sources live beside this script, so it works from any cwd (CI runs it from
# the fork root, the justfile from herdr-tools/).
src="$(cd "$(dirname "$0")/.." && pwd -P)"
pkg="${out}/shadowfax.tools"

if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    err "not an executable file: ${binary}"
fi
for file in herdr-plugin.toml README.md LICENSE; do
    [ -f "${src}/${file}" ] || err "missing ${src}/${file}"
done
[ -d "${src}/nvim" ] || err "missing ${src}/nvim"
# Refuse to merge into an old layout: a stale file left behind would ship.
[ ! -e "$pkg" ] || err "already exists: ${pkg}"

mkdir -p "${pkg}/bin"
cp "${src}/herdr-plugin.toml" "${src}/README.md" "${src}/LICENSE" "${pkg}/"
cp "$binary" "${pkg}/bin/herdr-tools"
chmod 755 "${pkg}/bin/herdr-tools"
# nvim/<mode>/ holds runtime assets each mode loads from HERDR_PLUGIN_ROOT.
# .gitkeep only exists so git tracks the directory before a mode fills it.
cp -R "${src}/nvim" "${pkg}/nvim"
find "${pkg}/nvim" -name .gitkeep -type f -exec rm -f {} +
