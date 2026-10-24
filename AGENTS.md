# AGENTS.md

Compatibility: breaking-allowed

## Checks

Run `just build`, `just fmt-check`, `just lint` and `just test` before completing
changes. Tests live beside the code in `#[cfg(test)]` modules. Open work lives in
`backlog/`; remove completed items.

## Boundaries

- The adapter links two independent projects. open-weave's southbound protocol
  comes from `weave-core` and its uplink observation from `weave-reach`.
  spaghetti's IPC protocol comes from `spaghetti-common`. Do not redefine either
  protocol here.
- These are git dependencies pinned by `Cargo.lock`. After open-weave changes
  `PROTOCOL_VERSION` or spaghetti changes `IPC_VERSION`, run
  `cargo update -p weave-core -p weave-reach -p spaghetti-common`, adapt the
  adapter, and reinstall it with `just install`.

## Repository text

Comments are forbidden. Preserve executable directives such as shebangs. Put
CLI help in explicit argument metadata. Keep the README concise and check its
claims against code.
