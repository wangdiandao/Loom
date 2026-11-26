//! Loom desktop task manager binary entry point (composition root).

#![warn(missing_docs)]

mod bootstrap;
mod bridge;
mod features;
mod i18n;
mod shell;

use loom_pal::{NativeBackend, PlatformBackend};

rust_i18n::i18n!("locales", fallback = "en-US");

fn main() {
    // 1. Boost process scheduling priority immediately to stay responsive under 100% CPU load.
    NativeBackend::boost_self_priority();

    // 2. Initialize lightweight non-blocking tracing output.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    // 3. Parse optional CLI flags (`--restore-route`, `--config`, `--allow-multiple`) and run.
    let args = bootstrap::LaunchArgs::parse_from(std::env::args().skip(1));
    bootstrap::run(args);
}
