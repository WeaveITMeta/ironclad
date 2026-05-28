//! ARC-reactor centerpiece. SVG layered concentric rings with phase markers,
//! tick segments, and a central mic disc.
//!
//! Five visual states drive the look:
//! - `Idle`: slow rotation, dim glow.
//! - `Listening`: fast pulse, bright cyan, mic icon swapped for stop square.
//! - `Thinking`: outer ring sweeps faster, inner pulses out of phase.
//! - `Speaking`: inner ring expands/contracts to a slower wave.
//! - `Error`: rose-tinted halt state.

use leptos::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcState {
    Idle,
    Listening,
    Thinking,
    Speaking,
    Error,
}

impl ArcState {
    fn outer_class(self) -> &'static str {
        match self {
            ArcState::Idle => "animate-arc-spin",
            ArcState::Listening => "animate-arc-spin-fast",
            ArcState::Thinking => "animate-arc-spin-fast",
            ArcState::Speaking => "animate-arc-spin",
            ArcState::Error => "animate-arc-spin",
        }
    }
    fn middle_class(self) -> &'static str {
        match self {
            ArcState::Listening | ArcState::Thinking => "animate-arc-spin-rev",
            _ => "animate-arc-spin-rev",
        }
    }
    fn inner_pulse_class(self) -> &'static str {
        match self {
            ArcState::Idle => "animate-arc-pulse",
            ArcState::Listening => "animate-arc-pulse-fast",
            ArcState::Thinking => "animate-arc-pulse-fast",
            ArcState::Speaking => "animate-arc-pulse",
            ArcState::Error => "",
        }
    }
    fn ring_stroke(self) -> &'static str {
        // Cyan family for healthy states; amber (warn, not crimson) for
        // boot-time errors and recoverable faults. Reserves rose for
        // crash-level emergencies we don't surface here yet.
        match self {
            ArcState::Error => "#f59e0b",       // amber-500 — warn, not panic
            ArcState::Listening => "#22d3ee",
            ArcState::Thinking => "#22d3ee",
            ArcState::Speaking => "#67e8f9",
            ArcState::Idle => "#0e7490",
        }
    }
    fn core_fill(self) -> &'static str {
        match self {
            ArcState::Error => "rgba(245, 158, 11, 0.18)",
            _ => "rgba(34, 211, 238, 0.18)",
        }
    }
    fn label(self) -> &'static str {
        match self {
            ArcState::Idle => "STANDBY",
            ArcState::Listening => "LISTENING",
            ArcState::Thinking => "THINKING",
            ArcState::Speaking => "SPEAKING",
            ArcState::Error => "ERROR",
        }
    }
}

/// Big ARC-reactor SVG with embedded mic button.
///
/// The `on_click` bound deliberately omits `Send + Sync` because the click
/// handler captures !Send browser handles (`MediaRecorder`, etc.). CSR is
/// single-threaded so this is safe.
#[component]
pub fn ArcReactor<F: Fn() + 'static + Clone>(
    state: Signal<ArcState>,
    on_click: F,
) -> impl IntoView {
    view! {
        <div class="relative w-[300px] h-[300px] flex items-center justify-center">
            // Mode banner — floating above the reactor like a HUD readout.
            // Big tracking on small text reads as "official" rather than
            // "label". State color matches the reactor ring so the eye links them.
            <div class="absolute -top-10 left-0 right-0 text-center pointer-events-none">
                <div
                    class="inline-block text-[13px] font-mono tracking-[0.55em] px-3 py-1"
                    style=move || format!(
                        "color: {c}; text-shadow: 0 0 12px {c}; border-top: 1px solid {c}30; border-bottom: 1px solid {c}30;",
                        c = state.get().ring_stroke(),
                    )
                >
                    {move || state.get().label()}
                </div>
            </div>

            // Outer halo
            <div
                class=move || format!(
                    "absolute inset-0 rounded-full {}",
                    state.get().inner_pulse_class()
                )
                style=move || format!(
                    "box-shadow: 0 0 60px 10px {}, inset 0 0 30px {};",
                    match state.get() {
                        ArcState::Error => "rgba(245, 158, 11, 0.35)",
                        _ => "rgba(34, 211, 238, 0.35)",
                    },
                    match state.get() {
                        ArcState::Error => "rgba(245, 158, 11, 0.25)",
                        _ => "rgba(34, 211, 238, 0.25)",
                    },
                )
            ></div>

            // SVG: outer rotating ring with tick marks, middle ring, inner static ring
            <svg viewBox="0 0 200 200" class="absolute inset-0 w-full h-full">
                // Outer rotating ring (tick segments)
                <g
                    class=move || format!("origin-center {}", state.get().outer_class())
                    style="transform-origin: 50% 50%;"
                >
                    <circle cx="100" cy="100" r="92"
                        fill="none"
                        stroke=move || state.get().ring_stroke()
                        stroke-width="0.8"
                        stroke-dasharray="3 4"
                        opacity="0.7"
                    />
                    // Phase ticks every 30°
                    {(0..12).map(|i| {
                        let angle = i as f32 * 30.0;
                        view! {
                            <line
                                x1="100" y1="6" x2="100" y2="14"
                                stroke=move || state.get().ring_stroke()
                                stroke-width="1.4"
                                transform=format!("rotate({angle} 100 100)")
                            />
                        }
                    }).collect_view()}
                </g>

                // Middle counter-rotating ring with broken arc
                <g
                    class=move || format!("origin-center {}", state.get().middle_class())
                    style="transform-origin: 50% 50%;"
                >
                    <circle cx="100" cy="100" r="74"
                        fill="none"
                        stroke=move || state.get().ring_stroke()
                        stroke-width="1.2"
                        stroke-dasharray="40 12 60 12"
                        opacity="0.85"
                    />
                </g>

                // Inner static ring
                <circle cx="100" cy="100" r="56"
                    fill="none"
                    stroke=move || state.get().ring_stroke()
                    stroke-width="0.6"
                    opacity="0.6"
                />

                // Core disc
                <circle cx="100" cy="100" r="42"
                    fill=move || state.get().core_fill()
                    stroke=move || state.get().ring_stroke()
                    stroke-width="1.6"
                />
                // Inner glow
                <circle cx="100" cy="100" r="28"
                    fill="none"
                    stroke=move || state.get().ring_stroke()
                    stroke-width="0.4"
                    opacity="0.5"
                />
            </svg>

            // Center mic button (the actual click target)
            <button
                class=move || {
                    let base = "relative z-10 w-20 h-20 rounded-full \
                                flex items-center justify-center \
                                font-mono text-2xl select-none \
                                transition-all duration-200";
                    let state_class = match state.get() {
                        ArcState::Listening => "bg-arc-500/30 text-arc-100 ring-2 ring-arc-300 cursor-pointer",
                        ArcState::Error =>     "bg-amber-500/15 text-amber-200 ring-2 ring-amber-400/60 cursor-pointer",
                        _ =>                   "bg-arc-900/40 text-arc-300 ring-1 ring-arc-700 hover:bg-arc-800/50 cursor-pointer",
                    };
                    format!("{base} {state_class}")
                }
                on:click={
                    let cb = on_click.clone();
                    move |_| cb()
                }
            >
                {move || match state.get() {
                    ArcState::Listening => "■",
                    _ => "🎙",
                }}
            </button>

            // Subtle frequency/uptime readout below the reactor — gives the
            // HUD a sense of "live system" without saying anything specific.
            // The top mode banner is the primary label; this is texture.
            <div class="absolute -bottom-8 left-0 right-0 text-center pointer-events-none">
                <div
                    class="text-[9px] font-mono tracking-[0.45em] opacity-60"
                    style=move || format!("color: {};", state.get().ring_stroke())
                >
                    "// SYS · " {move || state.get().label()} " · IRONCLAW"
                </div>
            </div>
        </div>
    }
}
