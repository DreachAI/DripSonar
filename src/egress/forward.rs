//! One authenticated request or CONNECT tunnel per downstream connection.

use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, uri::Authority};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout, timeout_at},
};
use tokio_socks::tcp::Socks5Stream;
use url::Url;

use super::{FetchGuard, Route, RouteConfig, guard::GuardError};

const MAX_HEAD: usize = 32 * 1024;

pub struct ForwardProxy;

impl ForwardProxy {
    /// The token is the Basic-auth password; the username is `sonar`.
    /// Aborting the returned task also aborts its active connections.
    pub async fn start(
        fetch_guard: Arc<FetchGuard>,
        route_cfg: RouteConfig,
    ) -> io::Result<(SocketAddr, String, JoinHandle<()>)> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let authorization = format!("Basic {}", base64(format!("sonar:{token}").as_bytes()));
        let slots = Arc::new(Semaphore::new(route_cfg.concurrency));
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                while connections.try_join_next().is_some() {}
                tokio::select! {
                    result = listener.accept() => {
                        let (mut stream, _) = match result {
                            Ok(connection) => connection,
                            Err(error) => {
                                tracing::warn!(kind = ?error.kind(), "forward proxy accept failed");
                                if matches!(error.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::NotConnected) {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                continue;
                            }
                        };
                        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                            // A full queue must not create another waiting task or connection.
                            let _ = stream.try_write(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                            continue;
                        };
                        let guard = Arc::clone(&fetch_guard);
                        let routes = route_cfg.clone();
                        let authorization = authorization.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            let _ = serve(&mut stream, &guard, &routes, &authorization).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok((addr, token, task))
    }
}

async fn serve(
    client: &mut TcpStream,
    guard: &FetchGuard,
    routes: &RouteConfig,
    authorization: &str,
) -> io::Result<()> {
    let mut client = BufReader::new(client);
    let head = match timeout(routes.idle_timeout, read_head(&mut client)).await {
        Ok(Ok(head)) => head,
        _ => return respond(client.get_mut(), 400).await,
    };
    let mut auth = head.headers.get_all("proxy-authorization").iter();
    if !auth.next().is_some_and(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' '))
            .is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("Basic")
                    && authorization.strip_prefix("Basic ") == Some(token)
            })
    }) || auth.next().is_some()
    {
        return respond(client.get_mut(), 407).await;
    }
    let connect = head.method == Method::CONNECT;
    let destination = if connect {
        head.target
            .parse::<Authority>()
            .ok()
            .filter(|authority| authority.port_u16().is_some())
            .and_then(|authority| Url::parse(&format!("https://{authority}/")).ok())
    } else {
        Url::parse(&head.target)
            .ok()
            .filter(|url| url.scheme() == "http" && url.fragment().is_none())
    };
    let Some(url) = destination else {
        return respond(client.get_mut(), 400).await;
    };
    let framing = match body_framing(&head.headers) {
        Ok(framing) => framing,
        Err(_) => return respond(client.get_mut(), 400).await,
    };
    if connect && !matches!(framing, Framing::Length(0)) {
        return respond(client.get_mut(), 400).await;
    }
    let forward = if connect {
        Vec::new()
    } else {
        match forward_head(&head, &url, &framing) {
            Ok(bytes) => bytes,
            Err(_) => return respond(client.get_mut(), 400).await,
        }
    };
    let route = routes.route_for(url.host_str().unwrap_or_default());
    if routes.check_budget(route).is_err() {
        return respond(client.get_mut(), 403).await;
    }
    let addresses = match timeout(routes.idle_timeout, guard.resolve_url(&url)).await {
        Ok(Ok(addresses)) => addresses,
        Ok(Err(GuardError::Refused(_))) => return respond(client.get_mut(), 403).await,
        _ => return respond(client.get_mut(), 502).await,
    };
    let upstream = match timeout(routes.idle_timeout, dial(&addresses, routes, route)).await {
        Ok(Ok(stream)) => stream,
        _ => return respond(client.get_mut(), 502).await,
    };
    let (activity, mut observed) = watch::channel(Instant::now());
    let mut upstream = Metered {
        stream: upstream,
        activity,
        routes: routes.clone(),
        route,
    };
    let exchange = async {
        if connect {
            client
                .get_mut()
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        } else {
            upstream.write_all(&forward).await?;
            let (mut read, mut write) = tokio::io::split(upstream);
            let (read_client, mut write_client) = tokio::io::split(client);
            let mut read_client = BufReader::new(read_client);
            // Reading the response concurrently also supports Expect: 100-continue.
            tokio::try_join!(forward_body(&mut read_client, &mut write, framing), async {
                tokio::io::copy(&mut read, &mut write_client)
                    .await
                    .map(|_| ())
            },)?;
        }
        Ok(())
    };
    tokio::select! {
        result = exchange => result,
        () = idle(&mut observed, routes.idle_timeout) => Err(io::Error::new(io::ErrorKind::TimedOut, "proxy tunnel idle")),
    }
}

async fn dial(
    addresses: &[SocketAddr],
    routes: &RouteConfig,
    route: Route,
) -> io::Result<TcpStream> {
    for address in addresses {
        routes.check_budget(route).map_err(io::Error::other)?;
        let result = match route {
            Route::Direct => TcpStream::connect(address).await,
            Route::Proxy => {
                let upstream = routes
                    .upstream
                    .as_ref()
                    .ok_or_else(|| io::Error::other("proxy not configured"))?;
                let host = upstream
                    .url
                    .host_str()
                    .ok_or_else(|| io::Error::other("proxy has no host"))?;
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let endpoint = (host, upstream.url.port().unwrap_or(1080));
                let result = if upstream.username.is_empty() {
                    Socks5Stream::connect(endpoint, *address).await
                } else {
                    Socks5Stream::connect_with_password(
                        endpoint,
                        *address,
                        &upstream.username,
                        &upstream.password,
                    )
                    .await
                };
                result
                    .map(Socks5Stream::into_inner)
                    .map_err(|_| io::Error::other("SOCKS5 connection failed"))
            }
        };
        if let Ok(stream) = result {
            return Ok(stream);
        }
    }
    Err(io::Error::other("destination connection failed"))
}

struct Head {
    method: Method,
    target: String,
    headers: HeaderMap,
}

async fn read_head<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Head> {
    let mut remaining = MAX_HEAD;
    let line = read_line(reader, &mut remaining).await?;
    let line = std::str::from_utf8(&line).map_err(|_| malformed())?;
    let mut parts = line.split(' ');
    let method = Method::from_bytes(parts.next().ok_or_else(malformed)?.as_bytes())
        .map_err(|_| malformed())?;
    let target = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(malformed)?
        .to_owned();
    if !matches!(parts.next(), Some("HTTP/1.0" | "HTTP/1.1")) || parts.next().is_some() {
        return Err(malformed());
    }
    let mut headers = HeaderMap::new();
    loop {
        let line = read_line(reader, &mut remaining).await?;
        if line.is_empty() {
            break;
        }
        let (name, value) = parse_header(&line)?;
        headers.append(name, value);
    }
    Ok(Head {
        method,
        target,
        headers,
    })
}

async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    remaining: &mut usize,
) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = reader
        .take((*remaining + 1) as u64)
        .read_until(b'\n', &mut line)
        .await?;
    if read > *remaining || !line.ends_with(b"\r\n") {
        return Err(malformed());
    }
    *remaining -= read;
    line.truncate(line.len() - 2);
    Ok(line)
}

fn parse_header(line: &[u8]) -> io::Result<(HeaderName, HeaderValue)> {
    let colon = line
        .iter()
        .position(|byte| *byte == b':')
        .ok_or_else(malformed)?;
    let name = HeaderName::from_bytes(&line[..colon]).map_err(|_| malformed())?;
    let value = HeaderValue::from_bytes(line[colon + 1..].trim_ascii()).map_err(|_| malformed())?;
    Ok((name, value))
}

enum Framing {
    Length(u64),
    Chunked,
}

fn body_framing(headers: &HeaderMap) -> io::Result<Framing> {
    let length = headers.get_all("content-length").iter().collect::<Vec<_>>();
    let encoding = headers
        .get_all("transfer-encoding")
        .iter()
        .collect::<Vec<_>>();
    if length.len() > 1 || encoding.len() > 1 || (!length.is_empty() && !encoding.is_empty()) {
        return Err(malformed());
    }
    if let Some(encoding) = encoding.first() {
        return if encoding.as_bytes().eq_ignore_ascii_case(b"chunked") {
            Ok(Framing::Chunked)
        } else {
            Err(malformed())
        };
    }
    let length = match length.first() {
        Some(value) => {
            let value = value.to_str().map_err(|_| malformed())?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(malformed());
            }
            value.parse().map_err(|_| malformed())?
        }
        None => 0,
    };
    Ok(Framing::Length(length))
}

fn forward_head(head: &Head, url: &Url, framing: &Framing) -> io::Result<Vec<u8>> {
    let mut headers = head.headers.clone();
    let connections = headers
        .get_all("connection")
        .iter()
        .map(|value| value.to_str().map(str::to_owned))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| malformed())?;
    for value in connections {
        for name in value.split(',') {
            let name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| malformed())?;
            if matches!(
                name.as_str(),
                "host" | "content-length" | "transfer-encoding"
            ) {
                return Err(malformed());
            }
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "proxy-connection",
        "proxy-authorization",
        "proxy-authenticate",
        "keep-alive",
        "upgrade",
        "te",
        "trailer",
        "host",
        "content-length",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let authority = &url[url::Position::BeforeHost..url::Position::AfterPort];
    let mut bytes = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        head.method, path, authority
    )
    .into_bytes();
    match framing {
        Framing::Chunked => bytes.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
        Framing::Length(length) => {
            bytes.extend_from_slice(format!("Content-Length: {length}\r\n").as_bytes())
        }
    }
    for (name, value) in &headers {
        bytes.extend_from_slice(name.as_str().as_bytes());
        bytes.extend_from_slice(b": ");
        bytes.extend_from_slice(value.as_bytes());
        bytes.extend_from_slice(b"\r\n");
    }
    bytes.extend_from_slice(b"\r\n");
    Ok(bytes)
}

async fn forward_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut BufReader<R>,
    writer: &mut W,
    framing: Framing,
) -> io::Result<()> {
    match framing {
        Framing::Length(length) => copy_exact(reader, writer, length).await?,
        Framing::Chunked => loop {
            let mut remaining = MAX_HEAD;
            let line = read_line(reader, &mut remaining).await?;
            let size = line
                .split(|byte| *byte == b';')
                .next()
                .ok_or_else(malformed)?;
            if size.is_empty() || !size.iter().all(u8::is_ascii_hexdigit) {
                return Err(malformed());
            }
            let size = u64::from_str_radix(std::str::from_utf8(size).map_err(|_| malformed())?, 16)
                .map_err(|_| malformed())?;
            writer.write_all(format!("{size:x}\r\n").as_bytes()).await?;
            if size == 0 {
                let mut remaining = MAX_HEAD;
                loop {
                    let line = read_line(reader, &mut remaining).await?;
                    if line.is_empty() {
                        break;
                    }
                    parse_header(&line)?;
                }
                writer.write_all(b"\r\n").await?;
                break;
            }
            copy_exact(reader, writer, size).await?;
            let mut ending = [0; 2];
            reader.read_exact(&mut ending).await?;
            if ending != *b"\r\n" {
                return Err(malformed());
            }
            writer.write_all(b"\r\n").await?;
        },
    }
    writer.shutdown().await
}

async fn copy_exact<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    length: u64,
) -> io::Result<()> {
    if tokio::io::copy(&mut reader.take(length), writer).await? != length {
        return Err(malformed());
    }
    Ok(())
}

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed proxy request")
}

async fn respond(stream: &mut TcpStream, status: u16) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        407 => "Proxy Authentication Required",
        _ => "Bad Gateway",
    };
    let authenticate = if status == 407 {
        "Proxy-Authenticate: Basic realm=\"DripSonar\"\r\n"
    } else {
        ""
    };
    timeout(Duration::from_secs(1), stream.write_all(format!("HTTP/1.1 {status} {reason}\r\n{authenticate}Connection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()))
        .await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "proxy reply timeout"))?
}

async fn idle(activity: &mut watch::Receiver<Instant>, duration: Duration) {
    loop {
        let deadline = *activity.borrow_and_update() + duration;
        if !matches!(timeout_at(deadline, activity.changed()).await, Ok(Ok(()))) {
            return;
        }
    }
}

struct Metered {
    stream: TcpStream,
    activity: watch::Sender<Instant>,
    routes: RouteConfig,
    route: Route,
}

impl AsyncRead for Metered {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.routes.check_budget(self.route) {
            return Poll::Ready(Err(io::Error::other(error)));
        }
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buffer);
        if let Poll::Ready(Ok(())) = &result {
            let bytes = buffer.filled().len() - before;
            if bytes > 0 {
                self.activity.send_replace(Instant::now());
                if let Err(error) = self.routes.charge(self.route, bytes) {
                    return Poll::Ready(Err(io::Error::other(error)));
                }
            }
        }
        result
    }
}

impl AsyncWrite for Metered {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.routes.check_budget(self.route) {
            return Poll::Ready(Err(io::Error::other(error)));
        }
        let result = Pin::new(&mut self.stream).poll_write(cx, bytes);
        if let Poll::Ready(Ok(bytes)) = &result {
            if *bytes > 0 {
                self.activity.send_replace(Instant::now());
                if let Err(error) = self.routes.charge(self.route, *bytes) {
                    return Poll::Ready(Err(io::Error::other(error)));
                }
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            output.push(if index > chunk.len() {
                '='
            } else {
                char::from(ALPHABET[((word >> shift) & 63) as usize])
            });
        }
    }
    output
}
