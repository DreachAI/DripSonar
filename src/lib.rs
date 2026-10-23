//! DripSonar: a self-hosted scrape and search engine with Firecrawl-compatible endpoints.

pub mod api;
pub mod config;
pub mod egress;
pub mod net;
pub mod page;
pub mod scrape;
pub mod search;
pub mod store;

/// Local servers and a loopback-allowing fetcher for unit tests; no external network.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod testkit {
    use crate::{config::Config, net::Fetcher};

    /// Serves `app` on an ephemeral loopback port and returns a fetcher allowed to reach it,
    /// with `site.test` resolving to loopback and `private.test`/`mixed.test` to private
    /// answers, plus the server's base URL.
    pub async fn fetcher_for(app: axum::Router) -> (Fetcher, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let cfg = Config {
            bind: addr,
            data_dir: std::env::temp_dir(),
            api_key: None,
            user_agent: "DripSonar-test".into(),
            searxng_endpoint: None,
            extra_ports: vec![addr.port()],
            proxy: None,
            proxy_hosts: Vec::new(),
            proxy_max_bytes_per_day: None,
            forward_proxy_concurrency: 4,
            forward_proxy_idle_timeout_ms: 30_000,
            max_body_bytes: 1000,
            global_concurrency: 1,
            per_host_concurrency: 2,
            per_host_spacing_ms: 0,
            max_timeout_ms: 120_000,
            default_max_age_ms: 86_400_000,
        };
        let loopback = "127.0.0.1".parse().unwrap();
        let private = "10.0.0.1".parse().unwrap();
        let hosts = [
            ("site.test", vec![loopback]),
            ("private.test", vec![private]),
            ("mixed.test", vec![loopback, private]),
        ];
        let hosts = hosts
            .into_iter()
            .map(|(h, ips)| (h.to_owned(), ips))
            .collect();
        (
            Fetcher::for_tests(&cfg, hosts).unwrap(),
            format!("http://{addr}"),
        )
    }
}
