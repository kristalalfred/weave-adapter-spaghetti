label := "com.open-weave.adapter-spaghetti"
node_config := home_directory() / "Library/Application Support/open-weave/spaghetti-node.yaml"
northbound := "http://localhost:29080"
northbound_auth := "Authorization: Bearer " + env("WEAVE_NORTHBOUND_TOKEN", "bench-northbound-token")

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
    address=""
    if [ -z "$southbound" ]; then
        southbound="http://$(scutil --get LocalHostName | tr '[:upper:]' '[:lower:]').local:29081"
        address="$lan"
        WEAVE_SOUTHBOUND_KEY="${WEAVE_SOUTHBOUND_KEY:-bench-southbound-key-for-local-use-only}"
    fi
    dir="$(mktemp -d)"; trap 'rm -rf "$dir"' EXIT
    mkdir "$dir/node"
    cp "$daemon" "$(dirname "$daemon")/spaghetti" target/release/weave-adapter-spaghetti "$dir/node/"
    codesign --force --sign - "$dir/node/spaghettid" "$dir/node/weave-adapter-spaghetti" 2>/dev/null
    COPYFILE_DISABLE=1 tar --no-mac-metadata -czf "$dir/node.tar.gz" -C "$dir/node" .
    rm -rf "$dir/node"
    BUNDLE_URL="http://$lan:{{port}}/node.tar.gz" SOUTHBOUND_URL="$southbound" \
        SOUTHBOUND_ADDRESS="$address" SOUTHBOUND_KEY="$WEAVE_SOUTHBOUND_KEY" RELAY_NETWORK="$relay" \
        perl -pe 's/__(BUNDLE_URL|SOUTHBOUND_URL|SOUTHBOUND_ADDRESS|SOUTHBOUND_KEY|RELAY_NETWORK)__/$ENV{$1}/g' \
        install.sh >"$dir/install.sh"
    echo "nodes join the southbound at $southbound"
    echo "on any Mac, run:"
    echo "  curl -fsSL http://$lan:{{port}}/install.sh | bash"
    python3 -m http.server {{port}} -d "$dir" --bind "$lan"

[doc("Send <app>'s audio on this Mac to the built-in speakers of every other spaghetti node on the bench. Run it again to reach Macs that joined since.")]
send app="zen":
    #!/usr/bin/env bash
    set -euo pipefail
    stream="{{app}}-to-others"
    me="$(awk '/^  id:/ { print $2; exit }' "{{node_config}}" 2>/dev/null || true)"
    [ -n "$me" ] || { echo "this Mac has no installed node; install one with the script 'just installer' serves" >&2; exit 1; }
    nodes="$(curl -fsS -H "{{northbound_auth}}" {{northbound}}/nodes)"
    jq -e --arg me "$me" --arg app "{{app}}" 'any(.[].node; .id == $me and any(.inputs[]; .id == $app))' <<<"$nodes" >/dev/null \
        || { echo "$me has no running app {{app}}" >&2; exit 1; }
    receivers="$(jq -c --arg me "$me" '[.[].node
        | select(.id != $me and .status == "ready")
        | select(any(.capabilities.hop_profiles[]; .id == "srt-to-speaker" or .id == "webrtc-to-speaker"))
        | {node: .id, output: ([.outputs[] | select(.id | endswith("-speakers")) | .id] | first)}]' <<<"$nodes")"
    jq -r '.[] | select(.output == null) | "skipping \(.node): it has no built-in speakers"' <<<"$receivers" >&2
    body="$(jq -c --arg name "$stream" --arg me "$me" --arg app "{{app}}" '{
        name: $name,
        enabled: true,
        source: {input: {node: $me, id: $app}},
        destinations: [.[] | select(.output) | {id: .node, output: {node: .node, id: .output}}]
    }' <<<"$receivers")"
    to="$(jq -r '[.destinations[].output.node] | join(", ")' <<<"$body")"
    [ -n "$to" ] || { echo "no other Mac with speakers is on the bench yet" >&2; exit 1; }
    etag="$(curl -sS -D - -o /dev/null -H "{{northbound_auth}}" "{{northbound}}/streams/$stream" | awk 'tolower($1) == "etag:" { print $2 }' | tr -d '\r')"
    if [ -n "$etag" ]; then precondition="If-Match: $etag"; else precondition="If-None-Match: *"; fi
    reply="$(curl -sS -w '\n%{http_code}' -H "{{northbound_auth}}" -H "$precondition" -H 'Content-Type: application/json' -d "$body" {{northbound}}/streams)"
    [[ "$(tail -n1 <<<"$reply")" == 2* ]] || { echo "northbound refused $stream: $(sed '$d' <<<"$reply")" >&2; exit 1; }
    for _ in $(seq 1 30); do
        state="$(curl -fsS -H "{{northbound_auth}}" {{northbound}}/status | jq -c --arg name "$stream" '.streams[] | select(.name == $name)')"
        case "$(jq -r .status <<<"$state")" in
            flowing) echo "$stream: flowing to $to"; exit 0 ;;
            awaiting_input) echo "$stream: connected to $to, waiting for {{app}} to play"; exit 0 ;;
        esac
        sleep 2
    done
    echo "$stream: $(jq -r .status <<<"$state")" >&2
    jq -r '.conditions[] | select(.status == "false") | "  \(.detail)"' <<<"$state" >&2
    exit 1

[doc("Stop sending <app> to the others.")]
unsend app="zen":
    #!/usr/bin/env bash
    set -euo pipefail
    stream="{{app}}-to-others"
    etag="$(curl -sS -D - -o /dev/null -H "{{northbound_auth}}" "{{northbound}}/streams/$stream" | awk 'tolower($1) == "etag:" { print $2 }' | tr -d '\r')"
    [ -n "$etag" ] || { echo "$stream is not running"; exit 0; }
    curl -fsS -X DELETE -H "{{northbound_auth}}" -H "If-Match: $etag" "{{northbound}}/streams/$stream"
    echo "stopped $stream"
