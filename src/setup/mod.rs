//! Setup module.
//!
//! Setup runs entirely in the Leptos dashboard now. This module exposes the
//! HTTP backend (`run_onboard_mode`) that the dashboard drives, plus a handful
//! of channel helpers (`SecretsContext`, Telegram token validation) that the
//! wizard endpoints reuse to talk to provider APIs.
//!
//! The legacy stdin/stdout `SetupWizard` is gone — onboarding is browser-only.

mod channels;
mod onboard_api;

pub use channels::{SecretsContext, validate_telegram_token};
pub use onboard_api::run_onboard_mode;
