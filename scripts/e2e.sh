#!/bin/sh
# Runs the end-to-end tests (tests/e2e/run.py) against freshly built binaries.
#
# Needs python3, and sendspin-cli: set SENDSPIN_CLI to its path, have it on the
# PATH, or let this script download the pinned version for x86_64 Linux (which
# needs the libraries in its BUILD-INFO, e.g. libavahi-compat-libdnssd1).
set -eu
cd "$(dirname "$0")/.."

cargo build -q -p alosa-server -p alosa-agent

work=target/e2e
venv=$work/venv
if [ ! -x "$venv/bin/python" ]; then
    python3 -m venv "$venv"
fi
"$venv/bin/pip" install -q --disable-pip-version-check -r tests/e2e/requirements.txt

if [ -z "${SENDSPIN_CLI:-}" ]; then
    if command -v sendspin-cli >/dev/null; then
        SENDSPIN_CLI=$(command -v sendspin-cli)
    else
        version=$(cat crates/alosa-agent/sendspin-cli.version)
        dir=$work/sendspin-cli-$version-linux-x86_64
        if [ ! -x "$dir/usr/local/bin/sendspin-cli" ]; then
            echo "downloading sendspin-cli $version"
            base=https://github.com/Sendspin/sendspin-cpp-cli/releases/download/v$version
            curl -fSsL -o "$dir.tar.gz" "$base/sendspin-cli-$version-linux-x86_64.tar.gz"
            curl -fSsL -o "$work/SHA256SUMS" "$base/SHA256SUMS"
            (cd "$work" && grep " sendspin-cli-$version-linux-x86_64.tar.gz\$" SHA256SUMS | sha256sum -c -)
            tar xzf "$dir.tar.gz" -C "$work"
        fi
        SENDSPIN_CLI=$PWD/$dir/usr/local/bin/sendspin-cli
    fi
fi
export SENDSPIN_CLI

exec "$venv/bin/python" tests/e2e/run.py
