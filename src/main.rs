//! `sonar` command line: serve, scrape, search, export.

use std::{error::Error, io::IsTerminal, process::ExitCode, sync::Arc, time::Duration};

use clap::{Parser, Subcommand};
use dripsonar::{
    api,
    browser::BrowserTier,
    config::Config,
    net::Fetcher,
    page::RenderOptions,
    scrape::{self, ScrapeRequest, Scraper},
    search::Searcher,
    store::Store,
};
use tokio::time::Instant;

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
    /// Export successful capture history as JSON Lines (never overwrites an existing file).
    Export {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    #[command(hide = true)]
    ExtractPdf,
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

/// Serves until a signal; the browser tier stops after requests and jobs have drained. It
/// starts first, so job scrapes use it too.
async fn serve(cfg: &Config, fetcher: Fetcher) -> Result<(), Box<dyn Error>> {
    let state = api::AppState::new(cfg, fetcher.clone())?
        .start_browser(cfg, &fetcher)
        .await?;
    let browser = state.browser();
    let served = serve_until_signal(cfg, state).await;
    browser.shutdown().await;
    served
}

/// Stops accepting on a signal, then gives in-flight requests up to the longest request
/// timeout to finish.
async fn serve_until_signal(cfg: &Config, mut state: api::AppState) -> Result<(), Box<dyn Error>> {
    let mut dispatcher = state.start_jobs().await?;
    let jobs_state = state.clone();
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "serving");
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::clone(&stop);
    let server = axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async move { stopped.notified().await });
    let mut server = std::pin::pin!(server.into_future());
    tokio::select! {
        result = &mut server => {
            jobs_state.stop_jobs();
            dispatcher.await?;
            return Ok(result?);
        },
        result = &mut dispatcher => {
            jobs_state.stop_jobs();
            stop.notify_one();
            result?;
            return Err("job dispatcher stopped unexpectedly".into());
        },
        () = shutdown_signal() => {}
    }
    tracing::info!("draining in-flight requests");
    jobs_state.stop_jobs();
    stop.notify_one();
    let grace = Duration::from_millis(cfg.max_timeout_ms);
    let result = match tokio::time::timeout(grace, server).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("grace period elapsed; dropping remaining requests");
            Ok(())
        }
    };
    dispatcher.await?;
    Ok(result?)
}

async fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let cfg = cli.config;
    scrape::configure_rendering(cfg.render_threads);
    match cli.command {
        Command::ExtractPdf => dripsonar::document::extract_pdf_stdio()?,
        Command::Serve => serve(&cfg, Fetcher::new(&cfg)?).await?,
        Command::Scrape { url } => {
            let (fetcher, store) = (Fetcher::new(&cfg)?, Store::open(&cfg.data_dir)?);
            let browser = BrowserTier::start(&cfg, &fetcher, store.clone()).await?;
            let scraper = Scraper::new(fetcher, store, &cfg).with_browser(browser.clone());
            let request = ScrapeRequest {
                url,
                headers: Vec::new(),
                deadline: Instant::now() + scrape::DEFAULT_TIMEOUT,
                max_age: Duration::from_millis(cfg.default_max_age_ms),
                render: RenderOptions {
                    only_main_content: true,
                    remove_base64_images: true,
                    markdown: true,
                    ..RenderOptions::default()
                },
                raw_html: false,
            };
            let doc = scraper.scrape(&request).await;
            browser.shutdown().await;
            println!("{}", doc?.rendered.markdown.unwrap_or_default());
        }
        Command::Search { query } => {
            let deadline = Instant::now() + Duration::from_secs(60);
            let hits = Searcher::new(Fetcher::new(&cfg)?, &cfg)
                .search(&query, 5, "en", deadline)
                .await?;
            println!("{}", serde_json::to_string_pretty(&hits)?);
        }
        Command::Export { since, host, out } => {
            dripsonar::export::export(
                &Store::open(&cfg.data_dir)?,
                since.as_deref(),
                host.as_deref(),
                out.as_deref(),
            )
            .await?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "failed");
            ExitCode::FAILURE
        }
    }
}
