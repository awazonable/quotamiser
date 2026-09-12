//! The QuotaMiser server.
//!
//! Usage: `quotamiser [path/to/quotamiser.toml]`, defaulting to
//! `quotamiser.toml` in the working directory. Credentials come from the
//! environment variables the configuration names; a `.env` file beside the
//! configuration is read for any that are not already set.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use quotamiser_proxy::config::Config;
use quotamiser_proxy::runtime::Runtime;
use quotamiser_proxy::server;

#[tokio::main]
async fn main() -> ExitCode {
    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("quotamiser.toml"), PathBuf::from);
    match run(&path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("[quotamiser] {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(path: &Path) -> Result<(), String> {
    let config = Config::load(path).map_err(|error| error.to_string())?;
    if let Some(directory) = path.parent() {
        load_env_file(&directory.join(".env"));
    }
    let resolved = config.resolve().map_err(|error| error.to_string())?;
    let bind = resolved.bind;

    let runtime = Arc::new(
        Runtime::start(resolved)
            .await
            .map_err(|error| error.to_string())?,
    );
    let app = server::router(runtime.clone());
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|error| format!("could not bind {bind}: {error}"))?;
    println!("[quotamiser] listening on http://{bind}");

    let serving = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("[quotamiser] stopping");
    });
    let result = serving.await.map_err(|error| error.to_string());
    runtime.shutdown().await;
    result
}

/// Reads `KEY=VALUE` lines into the environment, without overwriting anything
/// already set. Values are never logged.
fn load_env_file(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        // SAFETY: single-threaded startup, before any task is spawned.
        unsafe {
            std::env::set_var(key, value);
        }
    }
}
