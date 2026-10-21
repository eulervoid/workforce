# Vendored crates

Copies of upstream crates, unchanged except for what is listed here, used through
`[patch.crates-io]` in the workspace `Cargo.toml`. To bump one along with its dependent, unpack
the new upstream `.crate` here and apply the change again.

- `netwatch`: netwatch 0.19.3 (the version iroh 1.3 uses). Change: in
  `src/netmon/bsd.rs`, a refused `AF_ROUTE` socket means no watching of network changes, with a
  warning, instead of an error. Why: iroh opens that socket on every endpoint bind on macOS, and
  Fence refuses it, so without the change no endpoint binds in the agents' sandbox and the
  network tests cannot run there.
