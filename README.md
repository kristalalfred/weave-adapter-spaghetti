# weave-adapter-spaghetti

Joins a Mac running [spaghetti](https://github.com/kristalalfred/spaghetti) to an
[open-weave](https://github.com/kristalalfred/open-weave) control plane as a
node. The Mac's running apps become audio inputs, its output devices become
outputs, and the hops the controller plans for it become spaghetti sources and
routes.

## Install a node

A node needs macOS 14.2 or later on Apple silicon, with spaghetti installed.

```sh
just installer
```

This serves an installer on the LAN and prints the command to run on each Mac:

```sh
curl -fsSL http://<lan-address>:29097/install.sh | bash
```

Nodes join the open-weave bench southbound. Set `WEAVE_SOUTHBOUND_URL` and
`WEAVE_SOUTHBOUND_KEY` to join another one, and pass `relay_network=<network>`
to also dial a relay network. The installer runs the adapter as the LaunchAgent
`com.open-weave.adapter-spaghetti` and logs to
`~/Library/Logs/open-weave/adapter-spaghetti.log`. `just install` rebuilds the
adapter and restarts this Mac's node.

To run the adapter by hand, `just run <config>` takes a config like
[`examples/spaghetti-node.yaml`](examples/spaghetti-node.yaml).

## Hop profiles

| Profile | Ingress | Egresses |
|---|---|---|
| `app-audio-out` | an app input | up to eight, each SRT or WebRTC |
| `srt-to-speaker` | SRT carrying AAC in MPEG-TS | one output device |
| `webrtc-to-speaker` | WebRTC carrying Opus in RTP | one output device |

Every profile advertises `unknown` media behaviour. The adapter does not merge
two paths or run timed delivery.

Each running app under `/Applications`, `/System/Applications` or
`~/Applications` is an input, and each output device is an output, with ids made
from their names.
