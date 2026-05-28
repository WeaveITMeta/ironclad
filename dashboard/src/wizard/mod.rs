//! Browser-driven onboarding wizard.
//!
//! On dashboard mount, `App` calls `/api/onboard/status`. If onboarding is not
//! complete, `Wizard` is rendered instead of the dashboard pillars. Six steps,
//! one POST each, then a final `complete` call that flips the persisted flag
//! and prompts the user to restart Iron Clad in full mode.

pub mod api;

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use api::{
    AnthropicBody, ChannelsBody, HeartbeatBody, ModelBody, SecurityBody, post_anthropic,
    post_channels, post_complete, post_heartbeat, post_model, post_security,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Security,
    Anthropic,
    Model,
    Channels,
    Heartbeat,
    Done,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Security => "Security",
            Step::Anthropic => "Claude key",
            Step::Model => "Model",
            Step::Channels => "Channels",
            Step::Heartbeat => "Heartbeat",
            Step::Done => "Done",
        }
    }
    fn index(self) -> usize {
        match self {
            Step::Security => 1,
            Step::Anthropic => 2,
            Step::Model => 3,
            Step::Channels => 4,
            Step::Heartbeat => 5,
            Step::Done => 6,
        }
    }
}

#[component]
pub fn Wizard() -> impl IntoView {
    let step = RwSignal::new(Step::Security);

    let advance = move |to: Step| step.set(to);

    view! {
        <div class="min-h-screen bg-gradient-to-br from-zinc-950 via-zinc-900 to-zinc-950 text-zinc-100">
            <div class="max-w-2xl mx-auto px-6 py-12">
                <WizardHeader step />

                {move || match step.get() {
                    Step::Security => view! { <StepSecurity on_done=move || advance(Step::Anthropic) /> }.into_any(),
                    Step::Anthropic => view! { <StepAnthropic on_done=move || advance(Step::Model) /> }.into_any(),
                    Step::Model => view! { <StepModel on_done=move || advance(Step::Channels) /> }.into_any(),
                    Step::Channels => view! { <StepChannels on_done=move || advance(Step::Heartbeat) /> }.into_any(),
                    Step::Heartbeat => view! { <StepHeartbeat on_done=move || advance(Step::Done) /> }.into_any(),
                    Step::Done => view! { <StepDone /> }.into_any(),
                }}
            </div>
        </div>
    }
}

#[component]
fn WizardHeader(step: RwSignal<Step>) -> impl IntoView {
    view! {
        <header class="mb-8">
            <div class="text-[11px] font-mono uppercase tracking-widest text-jarvis-400">"Iron Clad setup"</div>
            <h1 class="text-3xl font-bold mt-1">"Bring JARVIS online"</h1>
            <p class="text-sm text-zinc-400 mt-2">
                "Five quick steps. Settings save as you go; you can re-run the wizard later."
            </p>
            <div class="mt-6 flex items-center gap-2 text-[11px] font-mono text-zinc-500">
                {move || {
                    let s = step.get();
                    let total = 5usize;
                    let current = s.index().min(total);
                    let pct = ((current as f32 / total as f32) * 100.0) as u32;
                    let label = if s == Step::Done {
                        "complete".to_string()
                    } else {
                        format!("step {current}/{total} — {}", s.label())
                    };
                    view! {
                        <div class="flex-1 h-1 rounded-full bg-zinc-800 overflow-hidden">
                            <div
                                class="h-1 bg-jarvis-500 transition-all"
                                style=format!("width: {pct}%")
                            ></div>
                        </div>
                        <span>{label}</span>
                    }
                }}
            </div>
        </header>
    }
}

// =============================================================================
// Step 1: Security
// =============================================================================

#[component]
fn StepSecurity<F: Fn() + Send + Sync + 'static + Copy>(on_done: F) -> impl IntoView {
    let source = RwSignal::new("keychain".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let generated = RwSignal::new(None::<String>);

    let submit = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = SecurityBody { source: source.get() };
        spawn_local(async move {
            match post_security(body).await {
                Ok(resp) => {
                    generated.set(resp.generated_key);
                    busy.set(false);
                    // Show the generated key for env mode; auto-advance for keychain/none.
                    if generated.get().is_none() {
                        on_done();
                    }
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    view! {
        <StepFrame title="Step 1 — Secrets master key">
            <p class="text-sm text-zinc-400">
                "Iron Clad encrypts saved tokens (Telegram, Anthropic, etc.) with a 256-bit key. "
                "Choose where to keep it."
            </p>
            <RadioGroup
                signal=source
                options=vec![
                    ("keychain", "OS keychain (recommended for local installs)"),
                    ("env", "Environment variable (best for CI/Docker)"),
                    ("none", "Skip — secrets features stay off"),
                ]
            />
            { move || generated.get().map(|key| view! {
                <div class="mt-4 p-3 rounded border border-yellow-700 bg-yellow-900/30 text-xs font-mono">
                    <div class="text-yellow-400 mb-1">"Add to your shell profile:"</div>
                    <div class="break-all">{format!("export SECRETS_MASTER_KEY={key}")}</div>
                    <button
                        class="mt-2 text-yellow-300 underline"
                        on:click=move |_| on_done()
                    >
                        "I added it — continue"
                    </button>
                </div>
            }) }
            <ErrorRow error />
            <PrimaryButton busy label="Continue" on_click=submit />
        </StepFrame>
    }
}

// =============================================================================
// Step 2: Anthropic API key
// =============================================================================

#[component]
fn StepAnthropic<F: Fn() + Send + Sync + 'static + Copy>(on_done: F) -> impl IntoView {
    let key = RwSignal::new(String::new());
    let base = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let submit = move |_| {
        if busy.get() {
            return;
        }
        let raw_key = key.get();
        if !raw_key.trim().starts_with("sk-ant-") {
            error.set(Some("Anthropic API keys start with sk-ant-".to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = AnthropicBody {
            api_key: raw_key,
            base_url: {
                let b = base.get();
                if b.trim().is_empty() { None } else { Some(b) }
            },
        };
        spawn_local(async move {
            match post_anthropic(body).await {
                Ok(_) => {
                    busy.set(false);
                    on_done();
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    view! {
        <StepFrame title="Step 2 — Anthropic API key">
            <p class="text-sm text-zinc-400">
                "Get a key from "
                <a href="https://console.anthropic.com" class="text-jarvis-400 underline" target="_blank">"console.anthropic.com"</a>
                ". Starts with "
                <span class="font-mono text-zinc-300">"sk-ant-..."</span>
                ". Saved to .env."
            </p>
            <LabeledField label="API key">
                <input
                    type="password"
                    class=input_class()
                    placeholder="sk-ant-..."
                    on:input=move |ev| key.set(event_target_value(&ev))
                />
            </LabeledField>
            <LabeledField label="Base URL (optional)">
                <input
                    type="text"
                    class=input_class()
                    placeholder="https://api.anthropic.com"
                    on:input=move |ev| base.set(event_target_value(&ev))
                />
            </LabeledField>
            <ErrorRow error />
            <PrimaryButton busy label="Save key and continue" on_click=submit />
        </StepFrame>
    }
}

// =============================================================================
// Step 3: Model selection
// =============================================================================

#[component]
fn StepModel<F: Fn() + Send + Sync + 'static + Copy>(on_done: F) -> impl IntoView {
    let model = RwSignal::new("claude-sonnet-4-6".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let submit = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = ModelBody { model: model.get() };
        spawn_local(async move {
            match post_model(body).await {
                Ok(_) => {
                    busy.set(false);
                    on_done();
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    view! {
        <StepFrame title="Step 3 — Pick a Claude model">
            <p class="text-sm text-zinc-400">"You can switch later from config."</p>
            <RadioGroup
                signal=model
                options=vec![
                    ("claude-sonnet-4-6", "Sonnet 4.6 — balanced default"),
                    ("claude-opus-4-5", "Opus 4.5 — highest quality"),
                    ("claude-haiku-4-5", "Haiku 4.5 — fastest and cheapest"),
                ]
            />
            <LabeledField label="Or a custom model ID">
                <input
                    type="text"
                    class=input_class()
                    placeholder="claude-sonnet-4-6-20251001"
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        if !v.trim().is_empty() {
                            model.set(v);
                        }
                    }
                />
            </LabeledField>
            <ErrorRow error />
            <PrimaryButton busy label="Save model and continue" on_click=submit />
        </StepFrame>
    }
}

// =============================================================================
// Step 4: Channels
// =============================================================================

#[component]
fn StepChannels<F: Fn() + Send + Sync + 'static + Copy>(on_done: F) -> impl IntoView {
    let tunnel_url = RwSignal::new(String::new());
    let http_enabled = RwSignal::new(false);
    let telegram_enabled = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let submit = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        error.set(None);
        let url = tunnel_url.get();
        let mut wasm_channels = Vec::new();
        if telegram_enabled.get() {
            wasm_channels.push("telegram".to_string());
        }
        let body = ChannelsBody {
            tunnel_url: if url.trim().is_empty() { None } else { Some(url) },
            http_enabled: http_enabled.get(),
            http_port: if http_enabled.get() { Some(8080) } else { None },
            wasm_channels,
        };
        spawn_local(async move {
            match post_channels(body).await {
                Ok(_) => {
                    busy.set(false);
                    on_done();
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    view! {
        <StepFrame title="Step 4 — Channels">
            <p class="text-sm text-zinc-400">
                "CLI/TUI is always on. Toggle the rest. Telegram needs a bot token; "
                "set it after onboarding via "
                <span class="font-mono">"ironclad config set"</span>"."
            </p>
            <LabeledField label="Public tunnel URL (optional, e.g. ngrok)">
                <input
                    type="text"
                    class=input_class()
                    placeholder="https://abc123.ngrok.io"
                    on:input=move |ev| tunnel_url.set(event_target_value(&ev))
                />
            </LabeledField>
            <Toggle signal=http_enabled label="HTTP webhook channel (:8080)" />
            <Toggle signal=telegram_enabled label="Telegram bot (configure token later)" />
            <ErrorRow error />
            <PrimaryButton busy label="Continue" on_click=submit />
        </StepFrame>
    }
}

// =============================================================================
// Step 5: Heartbeat
// =============================================================================

#[component]
fn StepHeartbeat<F: Fn() + Send + Sync + 'static + Copy>(on_done: F) -> impl IntoView {
    let enabled = RwSignal::new(false);
    let minutes = RwSignal::new(30u64);
    let notify = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let submit = move |_| {
        if busy.get() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = HeartbeatBody {
            enabled: enabled.get(),
            interval_minutes: Some(minutes.get()),
            notify_channel: {
                let n = notify.get();
                if n.trim().is_empty() { None } else { Some(n) }
            },
        };
        spawn_local(async move {
            match post_heartbeat(body).await {
                Ok(_) => {
                    busy.set(false);
                    on_done();
                }
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };

    view! {
        <StepFrame title="Step 5 — Heartbeat">
            <p class="text-sm text-zinc-400">
                "Periodic background runs — e.g., scan inbox, check the calendar, watch for events."
            </p>
            <Toggle signal=enabled label="Enable heartbeat" />
            { move || enabled.get().then(|| view! {
                <>
                    <LabeledField label="Interval (minutes)">
                        <input
                            type="number"
                            class=input_class()
                            min="1"
                            value=move || minutes.get().to_string()
                            on:input=move |ev| {
                                if let Ok(n) = event_target_value(&ev).parse::<u64>() {
                                    minutes.set(n.max(1));
                                }
                            }
                        />
                    </LabeledField>
                    <LabeledField label="Notify on findings (optional channel name)">
                        <input
                            type="text"
                            class=input_class()
                            placeholder="telegram"
                            on:input=move |ev| notify.set(event_target_value(&ev))
                        />
                    </LabeledField>
                </>
            }) }
            <ErrorRow error />
            <PrimaryButton busy label="Finish setup" on_click=submit />
        </StepFrame>
    }
}

// =============================================================================
// Done
// =============================================================================

#[component]
fn StepDone() -> impl IntoView {
    let busy = RwSignal::new(true);
    let result = RwSignal::new(None::<Result<(), String>>);

    spawn_local(async move {
        match post_complete().await {
            Ok(_) => result.set(Some(Ok(()))),
            Err(e) => result.set(Some(Err(e))),
        }
        busy.set(false);
    });

    view! {
        <StepFrame title="Setup saved">
            { move || match (busy.get(), result.get()) {
                (true, _) => view! { <p class="text-sm text-zinc-400">"Saving and locking in..."</p> }.into_any(),
                (false, Some(Ok(_))) => view! {
                    <div>
                        <p class="text-sm text-zinc-300">
                            "All set. Restart Iron Clad to enter full mode:"
                        </p>
                        <pre class="mt-3 p-3 rounded bg-black/40 border border-zinc-800 text-xs font-mono">
                            "cargo run-jarvis"
                        </pre>
                        <p class="text-xs text-zinc-500 mt-3">
                            "Trunk stays running on :3000; Iron Clad will swap onboard mode for the full gateway on :3030."
                        </p>
                    </div>
                }.into_any(),
                (false, Some(Err(e))) => view! {
                    <div class="text-sm text-rose-400">{format!("Failed to finalize: {e}")}</div>
                }.into_any(),
                (false, None) => view! { <div></div> }.into_any(),
            }}
        </StepFrame>
    }
}

// =============================================================================
// Shared widgets
// =============================================================================

#[component]
fn StepFrame(title: &'static str, children: Children) -> impl IntoView {
    view! {
        <section class="rounded-xl border border-zinc-800 bg-zinc-900/40 p-6 space-y-4">
            <h2 class="text-lg font-semibold">{title}</h2>
            {children()}
        </section>
    }
}

#[component]
fn LabeledField(label: &'static str, children: Children) -> impl IntoView {
    view! {
        <label class="block space-y-1">
            <span class="text-[11px] font-mono uppercase tracking-wider text-zinc-500">{label}</span>
            {children()}
        </label>
    }
}

#[component]
fn ErrorRow(error: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        { move || error.get().map(|e| view! {
            <div class="text-xs text-rose-400 font-mono">{e}</div>
        }) }
    }
}

#[component]
fn PrimaryButton<F: Fn(leptos::ev::MouseEvent) + Send + Sync + 'static>(
    busy: RwSignal<bool>,
    label: &'static str,
    on_click: F,
) -> impl IntoView {
    view! {
        <button
            class="mt-2 px-4 py-2 rounded bg-jarvis-600 text-white text-sm font-semibold disabled:opacity-50 hover:bg-jarvis-500 transition-colors"
            prop:disabled=move || busy.get()
            on:click=on_click
        >
            { move || if busy.get() { "Saving..." } else { label } }
        </button>
    }
}

#[component]
fn Toggle(signal: RwSignal<bool>, label: &'static str) -> impl IntoView {
    view! {
        <label class="flex items-center gap-3 cursor-pointer select-none">
            <input
                type="checkbox"
                class="w-4 h-4 accent-jarvis-500"
                prop:checked=move || signal.get()
                on:change=move |ev| signal.set(event_target_checked(&ev))
            />
            <span class="text-sm text-zinc-200">{label}</span>
        </label>
    }
}

#[component]
fn RadioGroup(signal: RwSignal<String>, options: Vec<(&'static str, &'static str)>) -> impl IntoView {
    view! {
        <div class="space-y-2">
            { options.into_iter().map(|(value, label)| {
                let value_owned = value.to_string();
                let cloned = value_owned.clone();
                view! {
                    <label class="flex items-center gap-3 p-2 rounded border border-zinc-800 hover:border-zinc-700 cursor-pointer">
                        <input
                            type="radio"
                            class="accent-jarvis-500"
                            name="wizard-radio"
                            prop:checked=move || signal.get() == value_owned
                            on:change=move |_| signal.set(cloned.clone())
                        />
                        <span class="text-sm text-zinc-200">{label}</span>
                    </label>
                }
            }).collect_view() }
        </div>
    }
}

fn input_class() -> &'static str {
    "w-full mt-1 px-3 py-2 rounded bg-zinc-950 border border-zinc-800 text-sm font-mono \
     text-zinc-100 placeholder:text-zinc-600 focus:outline-none focus:border-jarvis-500"
}

fn event_target_value(ev: &web_sys::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .unwrap_or_default()
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}
