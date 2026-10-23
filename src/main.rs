//! `sonar` command line: serve, scrape, search.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dripsonar::{api, config::Config};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server.
    Serve,
    /// Scrape one URL and print its markdown.
    Scrape { url: String },
    /// Search the web and print the hits.
    Search { query: String },
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} },
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Serve => {
            let listener = tokio::net::TcpListener::bind(cli.config.bind).await?;
            tracing::info!(bind = %cli.config.bind, "serving");
            axum::serve(listener, api::router(api::AppState))
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            Ok(())
        }
        Command::Scrape { .. } | Command::Search { .. } => Err("not implemented yet".into()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "failed");
            ExitCode::FAILURE
        }
    }
}
