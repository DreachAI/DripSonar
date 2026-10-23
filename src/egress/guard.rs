//! Destination guard: public unicast addresses on allowed schemes and ports only.

#[cfg(any(test, feature = "test-support"))]
use std::collections::HashMap;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// Why a destination was refused. The resolver returns it through reqwest's error chain.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Refusal(pub(crate) String);

#[derive(Debug, thiserror::Error)]
#[error("dns resolution failed")]
pub(crate) struct DnsFailure;

#[derive(Debug, thiserror::Error)]
pub(super) enum GuardError {
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Dns(#[from] DnsFailure),
}

pub struct FetchGuard {
    extra_ports: Vec<u16>,
    /// Test-only: names resolve from this map alone, and loopback addresses are allowed.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) test_hosts: Option<HashMap<String, Vec<IpAddr>>>,
}

impl FetchGuard {
    pub fn new(extra_ports: &[u16]) -> Self {
        Self {
            extra_ports: extra_ports.to_vec(),
            #[cfg(any(test, feature = "test-support"))]
            test_hosts: None,
        }
    }

    /// Checks scheme, credentials, port, and IP-literal hosts, which bypass the resolver.
    pub(crate) fn check_url(&self, url: &Url) -> Result<(), Refusal> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Refusal(format!("scheme {} is not allowed", url.scheme())));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Refusal("credentials in URLs are not allowed".into()));
        }
        let port = url.port_or_known_default().unwrap_or(0);
        if port != 80 && port != 443 && !self.extra_ports.contains(&port) {
            return Err(Refusal(format!("port {port} is not allowed")));
        }
        match url.host() {
            Some(Host::Domain(_)) => Ok(()),
            Some(Host::Ipv4(ip)) => self.check_ip(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => self.check_ip(IpAddr::V6(ip)),
            None => Err(Refusal("missing host".into())),
        }
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), Refusal> {
        #[cfg(any(test, feature = "test-support"))]
        if self.test_hosts.is_some() && ip.is_loopback() {
            return Ok(());
        }
        if is_public(ip) {
            Ok(())
        } else {
            Err(Refusal(format!("address {ip} is not public")))
        }
    }

    async fn lookup(&self, host: &str) -> Result<Vec<SocketAddr>, DnsFailure> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hosts) = &self.test_hosts {
            let ips = hosts.get(host).ok_or(DnsFailure)?;
            return Ok(ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect());
        }
        let addrs = tokio::net::lookup_host((host, 0))
            .await
            .map_err(|_| DnsFailure)?;
        Ok(addrs.collect())
    }

    async fn checked_lookup(&self, host: &str) -> Result<Vec<SocketAddr>, GuardError> {
        let addrs = self.lookup(host).await?;
        if addrs.is_empty() {
            return Err(DnsFailure.into());
        }
        // Refuse mixed answers before any connection can race an unchecked address.
        for addr in &addrs {
            self.check_ip(addr.ip())?;
        }
        Ok(addrs)
    }

    pub(super) async fn resolve_url(&self, url: &Url) -> Result<Vec<SocketAddr>, GuardError> {
        self.check_url(url)?;
        let port = url.port_or_known_default().ok_or(DnsFailure)?;
        let mut addrs = match url.host() {
            Some(Host::Domain(host)) => self.checked_lookup(host).await?,
            Some(Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
            Some(Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
            None => return Err(DnsFailure.into()),
        };
        for addr in &mut addrs {
            addr.set_port(port);
        }
        Ok(addrs)
    }
}

/// The reqwest resolver: connections only ever use addresses this check accepted.
pub(crate) struct GuardedResolver(pub(crate) Arc<FetchGuard>);

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let guard = Arc::clone(&self.0);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs = guard.checked_lookup(&host).await.map_err(|error| {
                // Keep the resolver's original concrete errors available to callers.
                let error: Box<dyn std::error::Error + Send + Sync> = match error {
                    GuardError::Refused(error) => Box::new(error),
                    GuardError::Dns(error) => Box::new(error),
                };
                error
            })?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Public unicast only; IPv6 forms that carry an IPv4 address are judged on that address.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match embedded_v4(v6) {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // this network
        || a == 10
        || a == 127
        || a >= 224 // multicast, reserved, broadcast
        || (a == 100 && (64..128).contains(&b)) // shared address space
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // protocol assignments, TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)) // TEST-NET-3
}

/// IPv4-mapped (`::ffff:0:0/96`), IPv4-compatible (`::/96`, including `::` and `::1`), and NAT64.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let nat64 = ip.segments()[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
    let low = Ipv4Addr::from((u128::from(ip) & 0xffff_ffff) as u32);
    ip.to_ipv4().or(nat64.then_some(low))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let [a, b, ..] = ip.segments();
    a & 0xe000 == 0x2000 // global unicast 2000::/3; excludes ULA, link-local, multicast
        && !(a == 0x2001 && b < 0x0200) // IETF special purpose: Teredo, benchmarking, ORCHID
        && !(a == 0x2001 && b == 0x0db8) // documentation
        && a != 0x2002 // 6to4
        && !(a == 0x3fff && b < 0x1000) // documentation
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::FetchGuard as Guard;
    use super::*;

    #[tokio::test]
    async fn guard_table() {
        let refused = [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "2001::1",
            "2001:db8::1",
            "2002:c000:201::1",
            "3fff::1",
        ];
        for ip in refused {
            assert!(!is_public(ip.parse().unwrap()), "{ip} must be refused");
        }
        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ];
        for ip in allowed {
            assert!(is_public(ip.parse().unwrap()), "{ip} must be allowed");
        }

        let guard = Guard::new(&[8443]);
        for url in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "http://user:pw@example.com/",
            "http://example.com:8080/",
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://[::ffff:169.254.169.254]/",
            "http://[fe80::1]/",
        ] {
            assert!(guard.check_url(&url.parse().unwrap()).is_err(), "{url}");
        }
        for url in [
            "http://example.com/",
            "https://example.com:443/",
            "https://example.com:8443/",
            "http://8.8.8.8/",
        ] {
            assert!(guard.check_url(&url.parse().unwrap()).is_ok(), "{url}");
        }

        let mut guard = Guard::new(&[]);
        let hosts = [
            ("public.test", vec!["8.8.8.8".parse().unwrap()]),
            (
                "mixed.test",
                vec!["8.8.8.8".parse().unwrap(), "10.0.0.1".parse().unwrap()],
            ),
        ];
        guard.test_hosts = Some(hosts.into_iter().map(|(h, ips)| (h.into(), ips)).collect());
        let resolver = GuardedResolver(Arc::new(guard));
        let resolve = |name: &str| resolver.resolve(name.parse().unwrap());
        assert!(resolve("public.test").await.is_ok());
        let mixed = resolve("mixed.test").await.err().unwrap();
        assert!(
            mixed.is::<Refusal>(),
            "a mixed public/private answer is refused"
        );
        assert!(
            resolve("missing.test")
                .await
                .err()
                .unwrap()
                .is::<DnsFailure>()
        );
    }
}
