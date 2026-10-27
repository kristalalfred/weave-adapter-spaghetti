#!/usr/bin/env bash
set -euo pipefail

bundle_url="__BUNDLE_URL__"
southbound_url="__SOUTHBOUND_URL__"
southbound_address="__SOUTHBOUND_ADDRESS__"
southbound_key="__SOUTHBOUND_KEY__"
relay_network="__RELAY_NETWORK__"
bin="$HOME/.local/bin"
support="$HOME/Library/Application Support/open-weave"
logs="$HOME/Library/Logs/open-weave"
agents="$HOME/Library/LaunchAgents"
label="com.open-weave.adapter-spaghetti"
plist="$agents/$label.plist"
config="$support/spaghetti-node.yaml"
log="$logs/adapter-spaghetti.log"
spaghetti_plist="$agents/com.spaghetti.daemon.plist"
domain="gui/$(id -u)"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }
plain() { sed $'s/\x1b\\[[0-9;]*m//g'; }

spaghetti_daemon() {
    plutil -extract ProgramArguments.0 raw "$spaghetti_plist" 2>/dev/null || true
}

place() {
    install -m 755 "$1" "$2.new"
    mv -f "$2.new" "$2"
}

uninstall() {
    launchctl bootout "$domain/$label" 2>/dev/null || true
    rm -f "$plist" "$bin/weave-adapter-spaghetti"
    rm -rf "$support" "$logs"
    if [ "$(spaghetti_daemon)" = "$bin/spaghettid" ]; then
        "$bin/spaghetti" daemon uninstall >/dev/null
        rm -f "$bin/spaghetti" "$bin/spaghettid"
    fi
    say "removed the open-weave spaghetti node"
}

check_platform() {
    [ "$(uname -s)" = Darwin ] || die "this installer is for macOS"
    [ "$(uname -m)" = arm64 ] || die "this build runs on Apple silicon Macs only"
    [ "$(id -u)" -ne 0 ] || die "run this as yourself, without sudo"
    local version major minor
    version="$(sw_vers -productVersion)"
    IFS=. read -r major minor _ <<<"$version.0.0"
    if [ "$major" -lt 14 ] || { [ "$major" -eq 14 ] && [ "$minor" -lt 2 ]; }; then
        die "spaghetti needs macOS 14.2 or later; this Mac runs $version"
    fi
}

choose_southbound() {
    local host
    host="${southbound_url#*://}"
    host="${host%%[:/]*}"
    if [ "$host" = "$(scutil --get LocalHostName | tr '[:upper:]' '[:lower:]').local" ]; then
        southbound_url="${southbound_url/$host/127.0.0.1}"
    elif [ -n "$southbound_address" ] && ! curl -fsS --max-time 5 -o /dev/null "$southbound_url/health" 2>/dev/null; then
        say "$host does not answer from this Mac; dialling $southbound_address, so a new address on that Mac takes a reinstall"
        southbound_url="${southbound_url/$host/$southbound_address}"
    fi
}

install_spaghetti() {
    local work="$1" existing
    existing="$(spaghetti_daemon)"
    if [ -n "$existing" ] && [ "$existing" != "$bin/spaghettid" ]; then
        say "keeping the spaghetti daemon at $existing"
        spaghetti="$(dirname "$existing")/spaghetti"
        return
    fi
    say "installing spaghetti into $bin"
    place "$work/spaghettid" "$bin/spaghettid"
    place "$work/spaghetti" "$bin/spaghetti"
    "$bin/spaghetti" daemon install >/dev/null
    spaghetti="$bin/spaghetti"
    for _ in $(seq 1 20); do
        "$spaghetti" status >/dev/null 2>&1 && return
        sleep 0.5
    done
    die "the spaghetti daemon did not start; see ~/Library/Logs/spaghetti/spaghettid.log"
}

node_id() {
    local wanted="${1:-}"
    if [ -z "$wanted" ] && [ -f "$config" ]; then
        wanted="$(awk '/^  id:/ { print $2; exit }' "$config")"
    fi
    [ -n "$wanted" ] || wanted="$(id -un)"
    printf '%s' "$wanted" | tr '[:upper:]' '[:lower:]' \
        | sed -e 's/[^a-z0-9][^a-z0-9]*/-/g' -e 's/^-//' | cut -c1-63 | sed 's/-$//'
}

node_token() {
    local mac
    mac="$(printf '%s' "$1.0" | openssl dgst -sha256 -hmac "$southbound_key" | awk '{ print $NF }')"
    printf '%s.0.%s' "$1" "$mac"
}

node_config() {
    local node="$1" token="$2"
    printf 'node:\n  id: %s\n  southbound_url: %s\n  southbound_token: %s\n' "$node" "$southbound_url" "$token"
    printf '  listen: 127.0.0.1:8092\n  topology:\n'
    printf '    site:\n      id: lan\n'
    printf '      listeners:\n        srt:\n          port_range: { start: 23000, end: 23099 }\n'
    if [ -n "$relay_network" ]; then
        printf '    attachments:\n      - { id: relay, network: %s, dial: true }\n' "$relay_network"
    fi
}

write_config() {
    (
        umask 077
        node_config "$@" >"$config.new"
    )
    mv -f "$config.new" "$config"
}

write_agent() {
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$label</string>
    <key>ProgramArguments</key>
    <array>
        <string>$bin/weave-adapter-spaghetti</string>
        <string>--config</string>
        <string>$config</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>$log</string>
    <key>StandardErrorPath</key>
    <string>$log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
EOF
}

start_adapter() {
    launchctl bootout "$domain/$label" 2>/dev/null || true
    for _ in $(seq 1 10); do
        launchctl bootstrap "$domain" "$plist" 2>/dev/null && return
        sleep 1
    done
    die "launchctl could not start $label"
}

probe() {
    "$spaghetti" receive "$1" 2>/dev/null | sed -n 's/.*id=\([0-9][0-9]*\).*/\1/p' || true
}

ask_for_network_access() {
    local gateway
    gateway="$(route -n get default 2>/dev/null | awk '/gateway:/ { print $2 }')"
    [ -z "$gateway" ] || probe "srt://$gateway:9"
    if /usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate 2>/dev/null | grep -q enabled; then
        probe "srt://:23099"
    fi
}

wait_for_registration() {
    local node="$1" from="$2" new
    for _ in $(seq 1 180); do
        new="$(tail -n +"$((from + 1))" "$log" | plain)"
        if grep -q "registered with southbound" <<<"$new"; then
            grep "registered with southbound" <<<"$new" | tail -1 \
                | sed -n 's/.*inputs=\([0-9]*\) outputs=\([0-9]*\).*/\1 inputs, \2 outputs/p'
            return
        fi
        if grep -q "cannot run; exiting" <<<"$new"; then
            tail -5 <<<"$new" >&2
            die "the adapter cannot run; see $log"
        fi
        sleep 1
    done
    tail -5 <<<"$new" >&2
    die "$node did not register within 3 minutes; see $log"
}

main() {
    if [ "${1:-}" = uninstall ]; then
        uninstall
        return
    fi
    check_platform
    choose_southbound
    local node from probes registered
    node="$(node_id "${1:-}")"
    [ -n "$node" ] || die "name this node: curl -fsSL ${bundle_url%/*}/install.sh | bash -s -- <name>"

    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    say "downloading the node bundle"
    curl -fsSL "$bundle_url" | tar -xz -C "$work"
    mkdir -p "$bin" "$support" "$logs" "$agents"

    install_spaghetti "$work"

    say "installing the open-weave adapter"
    place "$work/weave-adapter-spaghetti" "$bin/weave-adapter-spaghetti"
    write_config "$node" "$(node_token "$node")"
    write_agent
    touch "$log"
    from="$(wc -l <"$log" | tr -d ' ')"
    probes="$(ask_for_network_access)"
    start_adapter

    say "waiting for $node to register; allow network access if macOS asks"
    registered="$(wait_for_registration "$node" "$from")"
    sleep 5
    for probe in $probes; do
        "$spaghetti" unsource "$probe" >/dev/null 2>&1 || true
    done

    say "$node is on open-weave with $registered"
    echo "logs:      $log"
    echo "uninstall: curl -fsSL ${bundle_url%/*}/install.sh | bash -s uninstall"
}

main "$@"
