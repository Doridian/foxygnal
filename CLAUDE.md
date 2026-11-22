# foxygnal: notes for agents

Rust SDR flowgraph engine plus a Svelte browser UI. Read `docs/ARCHITECTURE.md` before touching
the runtime, and `docs/ROADMAP.md` for what's in scope.

## Commands

- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt`
- `cargo run -p foxygnal -- run examples/noisy-tone.fox.toml`
- `cargo run -p foxygnal -- serve` (UI must be built: `cd ui && npm run build`)
- UI: `cd ui && npm run check && npm run build`; dev server with `npm run dev` (proxies `/api` to :7355)

## Conventions

- Blocks live in `foxygnal-blocks`, never in core. A new block is a struct implementing `Block`
  plus a `BlockDescriptor` registered in its module's `register()`. The UI is generated from
  descriptors, so don't add per-block UI code.
- `work()` must never block indefinitely. Return `WorkStatus::Blocked` and let the scheduler park.
- Hot paths: no allocation, no locks, no per-sample slice re-fetching inside `work()`.
- Port type mismatches must produce errors that name both endpoints.
- The server binds loopback by default. Keep it that way.
- Repo is hosted on git.foxden.network (Forgejo): changes go through PRs, never push to `main`.
