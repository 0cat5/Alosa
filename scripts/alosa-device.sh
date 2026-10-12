#!/bin/sh
# alosa-device: installs, updates and looks after the Alosa agent on a Linux
# device (Raspberry Pi etc.). Installed as /usr/local/sbin/alosa-device.
#
#   alosa-device install [options]    install the agent and sendspin-cli and start it
#   alosa-device update [options]     update both (rolls back if the new agent won't start)
#   alosa-device status               versions, service state, pairing
#   alosa-device reset                forget the paired server so the device can be adopted again
#   alosa-device auto-update on|off   check for updates once a week
#   alosa-device uninstall [--purge]  remove it (--purge also deletes its keys and settings)
#
# Options for install and update:
#   --version X.Y.Z   install that release instead of the latest
#   --from DIR        use release files from DIR instead of downloading
#                     (alosa-agent-*.tar.gz and release.env; optionally SHA256SUMS
#                     and sendspin-cli-*.tar.gz)
#   --force           reinstall even if already up to date
#   --no-deps         don't apt-get install the libraries sendspin-cli needs
#   --quiet           only print errors (used by the auto-update timer)
# Install only:
#   --profile NAME    connect, speaker, display or eink-clock
#   --name NAME       the name the device suggests when you adopt it
#   --server URL      the Alosa server, if mDNS can't find it (ws://host:8765/device)
#
# First install on a fresh device:
#   curl -fsSL https://raw.githubusercontent.com/0cat5/Alosa/main/scripts/alosa-device.sh \
#     | sudo sh -s install --profile connect --name "Living Room"
set -eu

REPO=${ALOSA_REPO:-0cat5/Alosa}
SENDSPIN_REPO=Sendspin/sendspin-cpp-cli
# ALOSA_ROOT installs into another directory tree instead of /, for testing and
# packaging. It skips everything that touches the running system (apt,
# systemd, users).
ROOT=${ALOSA_ROOT:-}
BIN=$ROOT/usr/local/bin
SBIN=$ROOT/usr/local/sbin
ETC=$ROOT/etc
UNIT_DIR=$ETC/systemd/system
CONF=$ETC/alosa-agent.conf
STATE_DIR=$ROOT/var/lib/alosa-agent
SERVICE=alosa-agent.service
TIMER=alosa-device-update.timer

QUIET=
WORK=

say() { [ -n "$QUIET" ] || printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}
cleanup() { [ -z "$WORK" ] || rm -rf "$WORK"; }
trap cleanup EXIT
trap 'exit 130' INT TERM # exiting runs the EXIT trap

usage() {
    if [ -f "$0" ]; then
        sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
    else
        echo "usage: alosa-device install|update|status|reset|auto-update|uninstall"
    fi
    exit "${1:-1}"
}

live() { [ -z "$ROOT" ]; } # acting on the real system?

need_root() {
    if live && [ "$(id -u)" != 0 ]; then
        die "run this with sudo"
    fi
}

systemctl_() { if live; then systemctl "$@"; fi; }

# Maps the machine to the names release files use.
detect_arch() {
    case ${ALOSA_ARCH:-$(uname -m)} in
    x86_64 | amd64) echo x86_64 ;;
    aarch64 | arm64) echo arm64 ;;
    armv7l | armv7 | armhf) echo armv7 ;;
    armv6l | armv6) echo armv6 ;;
    *) die "unsupported machine: $(uname -m)" ;;
    esac
}

fetch() { # url dest
    if command -v curl >/dev/null; then
        curl -fSsL --retry 3 -o "$2" "$1"
    elif command -v wget >/dev/null; then
        wget -q -O "$2" "$1"
    else
        die "need curl or wget"
    fi
}

# The newest release's version, from where /releases/latest redirects to.
latest_version() { # repo
    url=https://github.com/$1/releases/latest
    if command -v curl >/dev/null; then
        location=$(curl -fsSI --max-time 15 "$url" | tr -d '\r' | sed -n 's/^[Ll]ocation: .*\/tag\/v\{0,1\}//p')
    else
        location=$(wget -q -S --max-redirect=0 -O /dev/null "$url" 2>&1 | sed -n 's/^ *[Ll]ocation: .*\/tag\/v\{0,1\}//p')
    fi
    [ -n "$location" ] || die "no published release of $1 found (none yet, or the repository is private); use --from DIR with release files instead"
    echo "$location"
}

# Checks downloaded files against SHA256SUMS, if there is one.
verify() { # dir file...
    dir=$1
    shift
    if [ ! -f "$dir/SHA256SUMS" ]; then
        warn "no SHA256SUMS, can't verify $*"
        return
    fi
    for f in "$@"; do
        expected=$(awk -v f="$f" '$2 == f || $2 == "*" f { print $1 }' "$dir/SHA256SUMS")
        [ -n "$expected" ] || die "$f is not listed in SHA256SUMS"
        actual=$(sha256sum "$dir/$f" | cut -d' ' -f1)
        [ "$expected" = "$actual" ] || die "$f is corrupt (checksum mismatch)"
    done
}

# Reads KEY=value lines we trust (no command substitution, just versions).
read_release_env() { # file
    ALOSA_VERSION=
    SENDSPIN_CLI_VERSION=
    while IFS='=' read -r key value; do
        case $value in *[!0-9A-Za-z.+-]*) die "unexpected value in release.env: $key" ;; esac
        case $key in
        ALOSA_VERSION) ALOSA_VERSION=$value ;;
        SENDSPIN_CLI_VERSION) SENDSPIN_CLI_VERSION=$value ;;
        esac
    done <"$1"
    [ -n "$ALOSA_VERSION" ] && [ -n "$SENDSPIN_CLI_VERSION" ] || die "release.env is incomplete"
}

# Puts the release files for $VERSION (or the latest) into $WORK.
get_release() {
    if [ -n "$FROM" ]; then
        [ -f "$FROM/release.env" ] || die "$FROM has no release.env"
        cp "$FROM/release.env" "$WORK/"
        [ ! -f "$FROM/SHA256SUMS" ] || cp "$FROM/SHA256SUMS" "$WORK/"
        read_release_env "$WORK/release.env"
        agent=alosa-agent-$ALOSA_VERSION-linux-$ARCH.tar.gz
        [ -f "$FROM/$agent" ] || die "$FROM has no $agent"
        cp "$FROM/$agent" "$WORK/"
    else
        [ -n "$VERSION" ] || VERSION=$(latest_version "$REPO")
        base=https://github.com/$REPO/releases/download/v$VERSION
        fetch "$base/release.env" "$WORK/release.env" || die "release v$VERSION not found"
        fetch "$base/SHA256SUMS" "$WORK/SHA256SUMS" || die "release v$VERSION has no SHA256SUMS"
        verify "$WORK" release.env
        read_release_env "$WORK/release.env"
        agent=alosa-agent-$ALOSA_VERSION-linux-$ARCH.tar.gz
        fetch "$base/$agent" "$WORK/$agent" || die "no $ARCH build in release v$VERSION"
    fi
    verify "$WORK" "$agent"
    tar xzf "$WORK/$agent" -C "$WORK"
    PKG=$WORK/alosa-agent-$ALOSA_VERSION-linux-$ARCH
}

installed_agent_version() { "$BIN/alosa-agent" --version 2>/dev/null | awk '{ print $2 }'; }
installed_sendspin_version() { "$BIN/sendspin-cli" --version 2>/dev/null | awk 'NR == 1 { print $2 }'; }

# Copies src to dest atomically, keeping the old file as dest.prev.
replace() { # src dest mode
    mkdir -p "$(dirname "$2")"
    cp "$1" "$2.new"
    chmod "$3" "$2.new"
    [ ! -f "$2" ] || cp -p "$2" "$2.prev"
    mv -f "$2.new" "$2"
}

# The sendspin-cli version this release was tested with.
install_sendspin() {
    if [ "$(installed_sendspin_version)" = "$SENDSPIN_CLI_VERSION" ] && [ -z "$FORCE" ]; then
        return
    fi
    say "installing sendspin-cli $SENDSPIN_CLI_VERSION"
    file=sendspin-cli-$SENDSPIN_CLI_VERSION-linux-$ARCH.tar.gz
    mkdir -p "$WORK/sendspin"
    if [ -n "$FROM" ] && [ -f "$FROM/$file" ]; then
        cp "$FROM/$file" "$WORK/sendspin/"
        [ ! -f "$FROM/sendspin-cli-SHA256SUMS" ] || cp "$FROM/sendspin-cli-SHA256SUMS" "$WORK/sendspin/SHA256SUMS"
    else
        base=https://github.com/$SENDSPIN_REPO/releases/download/v$SENDSPIN_CLI_VERSION
        fetch "$base/$file" "$WORK/sendspin/$file" || die "couldn't download $file"
        fetch "$base/SHA256SUMS" "$WORK/sendspin/SHA256SUMS" || die "couldn't download sendspin-cli's SHA256SUMS"
    fi
    verify "$WORK/sendspin" "$file"
    tar xzf "$WORK/sendspin/$file" -C "$WORK/sendspin"
    new=$WORK/sendspin/sendspin-cli-$SENDSPIN_CLI_VERSION-linux-$ARCH/usr/local/bin/sendspin-cli
    [ -f "$new" ] || die "$file doesn't contain sendspin-cli"
    replace "$new" "$BIN/sendspin-cli" 755
}

# Libraries sendspin-cli links against (Debian / Raspberry Pi OS names).
install_deps() {
    [ -z "$NO_DEPS" ] && live || return 0
    if ! command -v apt-get >/dev/null; then
        warn "not a Debian-based system: install ALSA, PortAudio, PulseAudio, PipeWire and Avahi's libdns_sd yourself"
        return
    fi
    alsa=libasound2t64
    apt-cache show "$alsa" >/dev/null 2>&1 || alsa=libasound2 # before Debian 13
    missing=
    for pkg in $alsa libportaudio2 libpulse0 libpipewire-0.3-0 libavahi-compat-libdnssd1 avahi-daemon; do
        dpkg -s "$pkg" >/dev/null 2>&1 || missing="$missing $pkg"
    done
    [ -n "$missing" ] || return 0
    say "installing libraries:$missing"
    apt-get update -qq
    # shellcheck disable=SC2086 # word splitting is intended
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends $missing >/dev/null
}

# Sets KEY="value" in the config file, replacing a (commented-out) existing line.
set_conf() { # key value
    case $2 in *'
'*) die "$1 can't contain a newline" ;; esac
    L="$1=\"$(printf '%s' "$2" | sed 's/[\\"]/\\&/g')\"" K=$1 awk '
        !done && $0 ~ "^#?" ENVIRON["K"] "=" { print ENVIRON["L"]; done = 1; next }
        { print }
        END { if (!done) print ENVIRON["L"] }
    ' "$CONF" >"$CONF.new"
    mv "$CONF.new" "$CONF"
}

install_files() {
    replace "$PKG/alosa-agent" "$BIN/alosa-agent" 755
    replace "$PKG/alosa-device" "$SBIN/alosa-device" 755
    replace "$PKG/systemd/alosa-agent.service" "$UNIT_DIR/$SERVICE" 644
    replace "$PKG/systemd/alosa-agent.sysusers" "$ETC/sysusers.d/alosa-agent.conf" 644
    rm -f "$UNIT_DIR/$SERVICE.prev" "$ETC/sysusers.d/alosa-agent.conf.prev" "$SBIN/alosa-device.prev"
    if [ ! -f "$CONF" ]; then
        mkdir -p "$ETC"
        cp "$PKG/systemd/alosa-agent.conf.example" "$CONF"
    fi
}

# Running the new binary before switching to it catches a wrong-architecture
# or corrupted download without touching the running service.
preflight() {
    "$PKG/alosa-agent" --version >/dev/null 2>&1 || die "the downloaded agent doesn't run on this machine ($ARCH)"
}

# After a restart: is the service still up a few seconds later? If not, put
# the previous binaries back.
check_or_roll_back() {
    live || return 0
    sleep 8
    if systemctl is-active --quiet "$SERVICE"; then
        return 0
    fi
    warn "the new agent didn't stay running; rolling back"
    for f in "$BIN/alosa-agent" "$BIN/sendspin-cli"; do
        [ ! -f "$f.prev" ] || mv -f "$f.prev" "$f"
    done
    systemctl restart "$SERVICE" || true
    die "update failed and was rolled back. See: journalctl -u $SERVICE -n 50"
}

parse_install_options() {
    VERSION='' FROM='' FORCE='' NO_DEPS='' PROFILE='' NAME='' SERVER=''
    while [ $# -gt 0 ]; do
        case $1 in
        --version) VERSION=${2#v} && shift ;;
        --from) FROM=$2 && shift ;;
        --force) FORCE=1 ;;
        --no-deps) NO_DEPS=1 ;;
        --quiet) QUIET=1 ;;
        --profile) PROFILE=$2 && shift ;;
        --name) NAME=$2 && shift ;;
        --server) SERVER=$2 && shift ;;
        -h | --help) usage 0 ;;
        *) die "unknown option: $1" ;;
        esac
        shift
    done
    case $PROFILE in "" | connect | speaker | display | eink-clock) ;; *) die "unknown profile: $PROFILE" ;; esac
}

cmd_install() {
    parse_install_options "$@"
    need_root
    ARCH=$(detect_arch)
    WORK=$(mktemp -d)
    get_release
    preflight
    say "installing Alosa agent $ALOSA_VERSION ($ARCH)"
    install_deps
    install_sendspin
    install_files
    [ -z "$PROFILE" ] || set_conf ALOSA_PROFILE "$PROFILE"
    [ -z "$NAME" ] || set_conf ALOSA_DEVICE_NAME "$NAME"
    [ -z "$SERVER" ] || set_conf ALOSA_SERVER "$SERVER"
    rm -f "$BIN/alosa-agent.prev" "$BIN/sendspin-cli.prev"

    if live; then
        systemd-sysusers
        systemctl daemon-reload
        # The agent runs sendspin-cli itself; its own service would conflict.
        if systemctl is-enabled --quiet sendspin-cli.service 2>/dev/null; then
            say "disabling sendspin-cli.service (the agent runs the player itself)"
            systemctl disable --now sendspin-cli.service
        fi
        systemctl enable --now "$SERVICE"
        systemctl restart "$SERVICE"
    fi
    say ""
    say "Installed. Settings: $CONF"
    say "Adopt the device in Home Assistant (Settings -> Devices & services -> Discovered)."
    say "Check on it with: sudo alosa-device status"
    say "Updates: sudo alosa-device update, or weekly with: sudo alosa-device auto-update on"
}

cmd_update() {
    parse_install_options "$@"
    need_root
    [ -x "$BIN/alosa-agent" ] || die "not installed yet; use: alosa-device install"
    ARCH=$(detect_arch)
    WORK=$(mktemp -d)
    get_release
    if [ "$(installed_agent_version)" = "$ALOSA_VERSION" ] &&
        [ "$(installed_sendspin_version)" = "$SENDSPIN_CLI_VERSION" ] && [ -z "$FORCE" ]; then
        say "already up to date ($ALOSA_VERSION)"
        return
    fi
    preflight
    say "updating Alosa agent $(installed_agent_version) -> $ALOSA_VERSION"
    install_deps
    install_sendspin
    install_files
    if live; then
        systemd-sysusers
        systemctl daemon-reload
        systemctl restart "$SERVICE"
    fi
    check_or_roll_back
    rm -f "$BIN/alosa-agent.prev" "$BIN/sendspin-cli.prev"
    say "updated to $ALOSA_VERSION"
}

# Loads ALOSA_* settings from the config file into the environment, the way
# the service sees them.
load_conf() {
    [ -f "$CONF" ] || return 0
    while IFS= read -r line; do
        case $line in
        ALOSA_*=*)
            key=${line%%=*}
            value=${line#*=}
            value=${value#\"}
            value=${value%\"}
            export "$key=$value"
            ;;
        esac
    done <"$CONF"
}

cmd_status() {
    [ -x "$BIN/alosa-agent" ] || die "not installed"
    say "agent:        $(installed_agent_version)"
    say "sendspin-cli: $(installed_sendspin_version || true)"
    if live; then
        say "service:      $(systemctl is-active "$SERVICE" 2>/dev/null || true)"
        say "auto-update:  $(systemctl is-enabled "$TIMER" 2>/dev/null | grep -q enabled && echo on || echo off)"
        if latest=$(latest_version "$REPO" 2>/dev/null); then
            if [ "$latest" = "$(installed_agent_version)" ]; then
                say "latest:       $latest (up to date)"
            else
                say "latest:       $latest (update with: sudo alosa-device update)"
            fi
        fi
    fi
    say ""
    (
        load_conf
        "$BIN/alosa-agent" --info --state-dir "$STATE_DIR" --sendspin-bin "$BIN/sendspin-cli"
    )
    if live; then
        say ""
        say "recent log (journalctl -u $SERVICE for more):"
        journalctl -u "$SERVICE" -n 5 --no-pager -o cat 2>/dev/null || true
    fi
}

cmd_reset() {
    need_root
    systemctl_ stop "$SERVICE"
    # The device keeps its own key; only the pairing and the server's config go.
    rm -f "$STATE_DIR/server.pub" "$STATE_DIR/config.pb"
    systemctl_ start "$SERVICE"
    say "Forgot the paired server. Adopt the device again in Home Assistant."
}

cmd_auto_update() {
    need_root
    case ${1:-} in
    on)
        mkdir -p "$UNIT_DIR"
        cat >"$UNIT_DIR/alosa-device-update.service" <<EOF
[Unit]
Description=Update the Alosa agent
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/alosa-device update --quiet
EOF
        cat >"$UNIT_DIR/$TIMER" <<EOF
[Unit]
Description=Weekly Alosa agent update

[Timer]
OnCalendar=weekly
RandomizedDelaySec=6h
Persistent=true

[Install]
WantedBy=timers.target
EOF
        systemctl_ daemon-reload
        systemctl_ enable --now "$TIMER"
        say "Automatic updates on (weekly). Turn off with: sudo alosa-device auto-update off"
        ;;
    off)
        systemctl_ disable --now "$TIMER" 2>/dev/null || true
        rm -f "$UNIT_DIR/$TIMER" "$UNIT_DIR/alosa-device-update.service"
        systemctl_ daemon-reload
        say "Automatic updates off."
        ;;
    *) die "usage: alosa-device auto-update on|off" ;;
    esac
}

cmd_uninstall() {
    need_root
    purge=
    [ "${1:-}" != --purge ] || purge=1
    systemctl_ disable --now "$TIMER" 2>/dev/null || true
    systemctl_ disable --now "$SERVICE" 2>/dev/null || true
    rm -f "$BIN/alosa-agent" "$BIN/alosa-agent.prev" "$BIN/sendspin-cli.prev" \
        "$UNIT_DIR/$SERVICE" "$UNIT_DIR/$TIMER" "$UNIT_DIR/alosa-device-update.service" \
        "$ETC/sysusers.d/alosa-agent.conf" "$SBIN/alosa-device"
    systemctl_ daemon-reload
    if [ -n "$purge" ]; then
        rm -rf "$CONF" "$STATE_DIR"
        say "Removed, including the device's keys and settings."
    else
        say "Removed. Kept $CONF and $STATE_DIR (the device's identity); --purge deletes them."
    fi
    say "sendspin-cli was left in $BIN; delete it if nothing else uses it."
}

command=${1:-}
[ $# -eq 0 ] || shift
case $command in
install) cmd_install "$@" ;;
update) cmd_update "$@" ;;
status) cmd_status ;;
reset) cmd_reset ;;
auto-update) cmd_auto_update "$@" ;;
uninstall) cmd_uninstall "$@" ;;
-h | --help | help) usage 0 ;;
*) usage ;;
esac
