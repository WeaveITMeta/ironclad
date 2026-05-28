//! App root. Bootstraps onboarding status, then either mounts the wizard or
//! the JARVIS HUD shell. The legacy pillar-card layout was retired when we
//! consolidated everything into the HUD.

use crate::jarvis::JarvisShell;
use crate::wizard::{Wizard, api::fetch_status};
use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Three-state load: still polling, definitely needs the wizard, or fully set up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnboardState {
    Loading,
    NeedsWizard,
    Ready,
}

#[component]
pub fn App() -> impl IntoView {
    let state = RwSignal::new(OnboardState::Loading);

    // Probe the onboard API on mount. If the call fails (gateway down on :3030)
    // assume we're past onboarding and go straight to the HUD.
    spawn_local(async move {
        match fetch_status().await {
            Ok(status) if !status.onboard_completed => state.set(OnboardState::NeedsWizard),
            Ok(_) => state.set(OnboardState::Ready),
            Err(_) => state.set(OnboardState::Ready),
        }
    });

    view! {
        { move || match state.get() {
            OnboardState::Loading => view! { <LoadingScreen /> }.into_any(),
            OnboardState::NeedsWizard => view! { <Wizard /> }.into_any(),
            OnboardState::Ready => view! { <JarvisShell /> }.into_any(),
        }}
    }
}

#[component]
fn LoadingScreen() -> impl IntoView {
    view! {
        <div class="min-h-screen flex items-center justify-center bg-hud-bg text-arc-500">
            <div class="text-[11px] font-mono uppercase tracking-[0.4em]">"booting jarvis..."</div>
        </div>
    }
}
