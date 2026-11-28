//! Loom desktop task manager binary entry point (composition root).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(missing_docs)]

mod bootstrap;
mod bridge;
mod features;
mod i18n;
mod shell;
mod startup_trace;

use loom_pal::{NativeBackend, PlatformBackend};

rust_i18n::i18n!("locales", fallback = "en-US");

fn main() {
    // 0. Fast-path early CLI dispatch for Windows persistent administrator startup brokers,
    //    IFEO Task Manager launcher, probes, and elevated maintenance helpers before initializing
    //    GPUI or the sampling engine.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(exit_code) = loom_pal::admin::try_handle_cli_mode(&raw_args) {
        std::process::exit(exit_code);
    }
    if let Some(exit_code) = loom_pal::taskmgr::try_handle_cli_mode(&raw_args) {
        std::process::exit(exit_code);
    }

    // 1. Parse optional CLI flags (`--restore-route`, `--config`, `--allow-multiple`,
    //    `--startup-trace`) and arm the opt-in startup tracer before any OS/UI work.
    let args = bootstrap::LaunchArgs::parse_from(&raw_args);
    startup_trace::init(args.startup_trace.clone());

    // 2. Boost process scheduling priority immediately to stay responsive under 100% CPU load.
    NativeBackend::boost_self_priority();
    startup_trace::mark("priority_boost");

    // 3. Initialize lightweight non-blocking tracing output.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    startup_trace::mark("tracing_init");

    // 4. Execute the progressive startup pipeline.
    bootstrap::run(args, raw_args);
}
