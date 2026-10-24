label := "com.open-weave.adapter-spaghetti"

default:
    @just --list

build:
    cargo build

test:
    cargo test

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

run config="examples/spaghetti-node.yaml":
    cargo run -- --config {{config}}

[doc("Rebuild the adapter into ~/.local/bin and restart this Mac's installed node.")]
install:
    #!/usr/bin/env bash
    set -euo pipefail
    plist="$HOME/Library/LaunchAgents/{{label}}.plist"
    [ -f "$plist" ] || { echo "this Mac has no installed node; install one with the script 'just installer' serves" >&2; exit 1; }
    cargo build --release --quiet
    codesign --force --sign - target/release/weave-adapter-spaghetti 2>/dev/null
    install -m 755 target/release/weave-adapter-spaghetti "$HOME/.local/bin/weave-adapter-spaghetti.new"
    mv -f "$HOME/.local/bin/weave-adapter-spaghetti.new" "$HOME/.local/bin/weave-adapter-spaghetti"
    launchctl kickstart -k "gui/$(id -u)/{{label}}"
    echo "restarted {{label}}; logs in ~/Library/Logs/open-weave/adapter-spaghetti.log"

[doc("Serve the node installer to the LAN; each Mac that runs it joins as a node named after its user and also dials <relay_network> when set. Nodes join the open-weave bench southbound, or the one in WEAVE_SOUTHBOUND_URL with the key in WEAVE_SOUTHBOUND_KEY.")]
installer port="29097" relay_network="":
    #!/usr/bin/env bash
    set -euo pipefail
    southbound="${WEAVE_SOUTHBOUND_URL:-}"
    if [ -n "$southbound" ]; then
        [[ "$southbound" =~ ^https?://[^[:space:]]+$ ]] \
            || { echo "WEAVE_SOUTHBOUND_URL $southbound is not an http or https URL" >&2; exit 1; }
        [ -n "${WEAVE_SOUTHBOUND_KEY:-}" ] \
            || { echo "WEAVE_SOUTHBOUND_URL is set without WEAVE_SOUTHBOUND_KEY: set the key of that southbound" >&2; exit 1; }
    fi
    relay="{{relay_network}}"
    if [ -n "$relay" ]; then
        [[ "$relay" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] \
            || { echo "relay_network $relay is not 1 to 63 lowercase letters, digits and inner hyphens" >&2; exit 1; }
        case "$relay" in
            site-*) echo "relay_network $relay starts with site-, which the controller reserves for sites" >&2; exit 1 ;;
        esac
        if [ -z "$southbound" ]; then
            case "$relay" in
                internet|docker-host) echo "relay_network $relay is refused: the bench Strom nodes listen on it at container addresses no Mac can reach" >&2; exit 1 ;;
            esac
        fi
    fi
    iface="$(route -n get default | awk '/interface:/ { print $2 }')"
    lan="$(ipconfig getifaddr "$iface" || true)"
    [ -n "$lan" ] || { echo "no LAN address on $iface" >&2; exit 1; }
    daemon="$(command -v spaghettid)" || { echo "spaghettid is not on PATH; install spaghetti first" >&2; exit 1; }
    launchctl plist __TEXT,__info_plist "$daemon" 2>/dev/null | grep -q NSLocalNetworkUsageDescription \
        || { echo "$daemon cannot ask for Local Network access; reinstall spaghetti" >&2; exit 1; }
    cargo build --release --quiet
    if [ -z "$southbound" ]; then
        southbound="http://$lan:29081"
        WEAVE_SOUTHBOUND_KEY="${WEAVE_SOUTHBOUND_KEY:-bench-southbound-key-for-local-use-only}"
    fi
    dir="$(mktemp -d)"; trap 'rm -rf "$dir"' EXIT
    mkdir "$dir/node"
    cp "$daemon" "$(dirname "$daemon")/spaghetti" target/release/weave-adapter-spaghetti "$dir/node/"
    codesign --force --sign - "$dir/node/spaghettid" "$dir/node/weave-adapter-spaghetti" 2>/dev/null
    COPYFILE_DISABLE=1 tar --no-mac-metadata -czf "$dir/node.tar.gz" -C "$dir/node" .
    rm -rf "$dir/node"
    BUNDLE_URL="http://$lan:{{port}}/node.tar.gz" SOUTHBOUND_URL="$southbound" \
        SOUTHBOUND_KEY="$WEAVE_SOUTHBOUND_KEY" RELAY_NETWORK="$relay" \
        perl -pe 's/__(BUNDLE_URL|SOUTHBOUND_URL|SOUTHBOUND_KEY|RELAY_NETWORK)__/$ENV{$1}/g' \
        install.sh >"$dir/install.sh"
    echo "nodes join the southbound at $southbound"
    echo "on any Mac, run:"
    echo "  curl -fsSL http://$lan:{{port}}/install.sh | bash"
    python3 -m http.server {{port}} -d "$dir" --bind "$lan"
