//! JARVIS dashboard entrypoint.
//!
//! Mounts the Leptos app onto `<body>`. Trunk serves this WASM bundle from
//! `localhost:3000` per `Trunk.toml`. Static-data phase (v0.1); future phases
//! replace `data::demo_*` with `gloo_net` calls into Iron Clad's axum server.

mod app;
mod jarvis;
mod wizard;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
