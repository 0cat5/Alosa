#!/bin/sh
# One version number for everything Alosa ships. It lives in three files:
#   Cargo.toml                             (server + agent)
#   custom_components/alosa/manifest.json  (Home Assistant integration)
#   addon/alosa/config.yaml                (add-on; must match its image tag)
#
# Usage:
#   scripts/version.sh               print the current version
#   scripts/version.sh check [TAG]   fail unless all three agree (and match TAG, e.g. v0.2.0)
#   scripts/version.sh set X.Y.Z     set all three
set -eu
cd "$(dirname "$0")/.."

cargo_version() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1; }
manifest_version() { sed -n 's/^ *"version": "\(.*\)".*$/\1/p' custom_components/alosa/manifest.json; }
addon_version() { sed -n 's/^version: "\(.*\)"$/\1/p' addon/alosa/config.yaml; }

case "${1:-}" in
"")
    cargo_version
    ;;
check)
    c=$(cargo_version) m=$(manifest_version) a=$(addon_version)
    if [ "$c" != "$m" ] || [ "$c" != "$a" ]; then
        echo "versions disagree: Cargo.toml=$c manifest.json=$m config.yaml=$a" >&2
        echo "fix with: scripts/version.sh set <version>" >&2
        exit 1
    fi
    if [ -n "${2:-}" ] && [ "v$c" != "$2" ]; then
        echo "tag $2 doesn't match version $c" >&2
        exit 1
    fi
    echo "$c"
    ;;
set)
    new=${2:?usage: scripts/version.sh set X.Y.Z}
    if ! echo "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'; then
        echo "not a version: $new" >&2
        exit 1
    fi
    # Only the first `version =` in Cargo.toml: the [workspace.package] one.
    sed -i "0,/^version = \".*\"$/s//version = \"$new\"/" Cargo.toml
    sed -i "s/^\( *\"version\": \)\".*\"/\1\"$new\"/" custom_components/alosa/manifest.json
    sed -i "s/^version: \".*\"$/version: \"$new\"/" addon/alosa/config.yaml
    # Refresh Cargo.lock so the release build (--locked) agrees.
    cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
    "$0" check
    ;;
*)
    sed -n '2,11p' "$0" >&2
    exit 1
    ;;
esac
