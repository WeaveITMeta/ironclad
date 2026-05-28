# Iron Clad Dashboard

> JARVIS dashboard. The dream toolkit, visualized. Leptos + Tailwind + Trunk on `localhost:3000`. Self-hosted, local-first.

## Stack

- **Leptos 0.7** (CSR; Rust to WASM)
- **Tailwind CSS** (via Trunk's built-in `tailwind-css` asset)
- **Trunk** (bundler + dev server)
- **Targets** (future phases): Iron Clad axum (`src/channels/web/`), Eustress MCP

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk
```

Trunk auto-fetches `wasm-bindgen` and the Tailwind CLI on first run. No npm required.

## Run

```bash
cd .ironclad/dashboard
trunk serve
```

Auto-opens `http://127.0.0.1:3000`.

## Build (production bundle)

```bash
trunk build --release
# Output in dist/
```

## Phases

| Phase | What lands | Status |
|-------|-----------|--------|
| **v0.1** | Static demo: four pillar cards (Tools, Agents, Communications, Memory), Objective Alpha strip, Eustress flame banner with manual fire-event toggle | **Built 2026-05-26** |
| **v0.2** | `/api/jarvis/state` from Iron Clad axum; real Fjall reads; replace mocked data | Designed |
| **v0.3** | Eustress MCP event subscription via SSE; flame lights on every MCP call | Designed |
| **v0.4** | Mission ticker; voice-channel status; agent write-back feed | Designed |

## File map

| Path | Holds |
|------|-------|
| `Cargo.toml` | Isolated workspace root; intentionally a sibling of `.ironclad/Cargo.toml`, not a member. Keeps upstream merges clean. |
| `Trunk.toml` | Build + serve config; port 3000, auto-open. |
| `index.html` | Entry; declares Tailwind + Rust WASM via `data-trunk` rel attrs. |
| `tailwind.config.js` | Theme: zinc base, jarvis-blue, eustress-amber, glow keyframes. |
| `style/tailwind.css` | Layer styles: pillar cards, status dots, badges. |
| `src/main.rs` | Mounts the Leptos app to body. |
| `src/app.rs` | Top-level layout: header, Objective Alpha strip, pillar grid, Eustress banner, footer. |
| `src/data.rs` | Mocked pillar items + status types. Replaced with HTTP fetch in v0.2. |
| `src/pillars/` | One component per pillar; shared `PillarCard`. |

## Vault spec

Living spec at [[00 System/JARVIS/Dashboard]] in the Obsidian vault.

## Why this layout

- **Sibling, not nested.** `dashboard/` has its own `[workspace]` table, so Cargo treats it as an independent root. Upstream merges of `nearai/ironclad` never touch dashboard.
- **CSR over SSR for v0.1.** No backend wiring needed to demo. v0.2 swaps to fetching JSON from Iron Clad's existing axum server.
- **Tailwind via Trunk.** No npm in the loop; Trunk handles Tailwind CLI download. Rust toolchain only.
