//! http.rs
//! core::web::http
//!
//! Tokio-free HTTP tier plus sync HTML extraction shared by the web tools and file-read isUrl.
//! Owns the SSRF boundary (per-hop private-address rejection, loopback allowed) and the body-size cap.
//! HTML converters live in core::html (dependency-free) and are re-exported here.
//!

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
#[cfg(not(windows))]
use ureq::unversioned::transport::DefaultConnector;
use ureq::unversioned::transport::NextTimeout;
#[cfg(windows)]
use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, Either, LazyBuffers,
    TcpConnector, Transport, TransportAdapter,
};

pub use crate::core::html::{
    ReadableDoc, html_links, html_readability, html_to_markdown, html_to_text,
};

pub const DEFAULT_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) ",
    "AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/126.0.0.0 Safari/537.36"
);
pub const DEFAULT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,application/json;q=0.8,text/plain;q=0.8,*/*;q=0.5";
pub const DEFAULT_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
// Client hints a real Chrome navigation carries. The brand versions must track the User-Agent's
// Chrome major (currently 126) or a strict bot filter treats the header pair as forged.
pub const DEFAULT_SEC_CH_UA: &str =
    "\"Not/A)Brand\";v=\"8\", \"Chromium\";v=\"126\", \"Google Chrome\";v=\"126\"";
pub const DEFAULT_SEC_CH_UA_PLATFORM: &str = "\"Windows\"";
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
// 렌더 응답용 기본 상한 20MB
pub const DEFAULT_MAX_BYTES: u64 = 20_000_000;
pub const DEFAULT_MAX_REDIRECTS: u32 = 5;
// Hard per-fetch memory ceiling: a caller-supplied maxBytes is clamped to this so no single
// request can buffer an unbounded body (release panic=abort would turn an alloc failure fatal).
pub const MAX_ALLOWED_BYTES: u64 = 200_000_000;
// 응답 전 연결 리셋 재시도 1회 (부작용 GET 보호)
const MAX_RESET_RETRIES: u32 = 1;

// 1. Fetch options / result ---------------------------------------------------
#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub timeout_ms: u64,
    pub max_bytes: u64,
    pub max_redirects: u32,
    pub user_agent: String,
}
impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            user_agent: DEFAULT_USER_AGENT.to_string(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct FetchedPage {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub final_url: String,
}
impl FetchedPage {
    pub fn is_html(&self) -> bool {
        self.content_type.to_ascii_lowercase().contains("html")
    }
    pub fn body_text(&self) -> String {
        decode_body(&self.content_type, &self.body)
    }
}
// 2. HTTP fetch with per-hop SSRF guard ---------------------------------------
// max_redirects is set to 0 on the agent so every 3xx is returned to us; we re-check each hop
// against the SSRF boundary before following, which closes the redirect-to-internal-host vector.
pub fn http_fetch(
    url: &str,
    opts: &FetchOptions,
    allow_private: bool,
) -> Result<FetchedPage, String> {
    let (mut response, final_url) = fetch_final(url, opts, allow_private)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let max_bytes = opts.max_bytes.min(MAX_ALLOWED_BYTES);
    let body = response
        .body_mut()
        .with_config()
        .limit(max_bytes)
        .read_to_vec()
        .map_err(|error| body_limit_error(max_bytes, &error.to_string()))?;
    Ok(FetchedPage {
        status,
        content_type,
        body,
        final_url,
    })
}
// Streaming variant: the body is copied straight into `writer` (bounded by max_bytes) instead
// of buffering the whole payload in memory — download-to-file uses this with a temp file.
#[derive(Clone, Debug)]
pub struct FetchMeta {
    pub status: u16,
    pub content_type: String,
    pub final_url: String,
    pub bytes: u64,
}
pub fn http_fetch_to_writer(
    url: &str,
    opts: &FetchOptions,
    allow_private: bool,
    writer: &mut dyn io::Write,
) -> Result<FetchMeta, String> {
    let (mut response, final_url) = fetch_final(url, opts, allow_private)?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let max_bytes = opts.max_bytes.min(MAX_ALLOWED_BYTES);
    // limit+1로 읽어 초과를 감지(버퍼링 경로의 limit 오류와 동일한 계약).
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(max_bytes.saturating_add(1))
        .reader();
    let copied = io::copy(&mut reader, writer).map_err(|error| {
        format!("Failed to read response body (limit {max_bytes} bytes): {error}")
    })?;
    if copied > max_bytes {
        return Err(body_limit_error(max_bytes, "body exceeds limit"));
    }
    Ok(FetchMeta {
        status,
        content_type,
        final_url,
        bytes: copied,
    })
}
// 공통 redirect 루프: 혼합 지연을 막기 위해 전체 체인에 하나의 wall-clock 예산을 적용한다.
fn fetch_final(
    url: &str,
    opts: &FetchOptions,
    allow_private: bool,
) -> Result<(ureq::http::Response<ureq::Body>, String), String> {
    let mut current = url.trim().to_string();
    let mut redirects = 0u32;
    // 표현 불가 timeoutMs 무기한 (덧셈 panic 방지)
    let deadline = Instant::now().checked_add(Duration::from_millis(opts.timeout_ms));
    loop {
        let response = fetch_hop(&current, opts, allow_private, deadline)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) && status != 304 {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if let Some(location) = location {
                if redirects >= opts.max_redirects {
                    return Err(format!(
                        "Too many HTTP redirects (> {})",
                        opts.max_redirects
                    ));
                }
                current = resolve_url(&current, &location);
                redirects += 1;
                continue;
            }
        }
        return Ok((response, current));
    }
}
fn fetch_hop(
    current: &str,
    opts: &FetchOptions,
    allow_private: bool,
    deadline: Option<Instant>,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let (parts, addrs) = resolve_and_check(current, allow_private)?;
    // Agent는 (scheme|host:port|검증된 IP set) 키로 캐시되어 연결·TLS 세션을 재사용한다
    // (go-fs-mcp transportFor 대응). DNS 응답이 바뀌면 키가 바뀌어 새 Agent를 받는다.
    // timeout은 요청 단위 config로 주입하므로 캐시된 Agent와 무관하다.
    // 검증된 주소는 Agent의 resolver에 그대로 고정되므로 연결이 DNS를 다시 조회하지
    // 않는다(검증-연결 사이 DNS 리바인딩 차단).
    let agent = cached_agent(&agent_cache_key(&parts, &addrs), &addrs);
    let mut resets = 0u32;
    loop {
        // 남은 전체 예산
        let remaining = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "Fetch exceeded total timeout of {}ms",
                        opts.timeout_ms
                    ));
                }
                Some(remaining)
            }
            None => None,
        };
        // Send the header set a real Chrome navigation carries. Servers that gate on
        // Sec-Fetch-* / client-hint presence (a common 403 bot filter) accept the request;
        // ureq still adds Host, Connection, and gzip Accept-Encoding on its own.
        let outcome = agent
            .get(current)
            .config()
            .timeout_global(remaining)
            .build()
            .header("User-Agent", opts.user_agent.as_str())
            .header("Accept", DEFAULT_ACCEPT)
            .header("Accept-Language", DEFAULT_ACCEPT_LANGUAGE)
            .header("Upgrade-Insecure-Requests", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
            .header("Sec-Ch-Ua", DEFAULT_SEC_CH_UA)
            .header("Sec-Ch-Ua-Mobile", "?0")
            .header("Sec-Ch-Ua-Platform", DEFAULT_SEC_CH_UA_PLATFORM)
            .call();
        // 응답 전 리셋 GET 재시도
        match outcome {
            Err(ureq::Error::Io(error)) if error.kind() == io::ErrorKind::ConnectionReset => {
                if resets < MAX_RESET_RETRIES {
                    resets += 1;
                    continue;
                }
                return Err(format!(
                    "HTTP request to {current} failed: {error} (connection reset before any response on both attempts; the GET was retried once)"
                ));
            }
            other => return other.map_err(|error| classify_hop_error(current, &error)),
        }
    }
}
// Separate a connection-level failure (firewall, offline host, DNS) from a response-level one so
// the caller can tell "the request never reached the server" from "the server answered".
fn classify_hop_error(url: &str, error: &ureq::Error) -> String {
    let detail = error.to_string();
    let lowered = detail.to_ascii_lowercase();
    let unreachable = lowered.contains("timeout")
        || lowered.contains("connect")
        || lowered.contains("dns")
        || lowered.contains("resolve")
        || lowered.contains("io error");
    if unreachable {
        format!(
            "HTTP request to {url} failed: {detail} (could not establish a connection to the host — \
             it is likely blocked by a network firewall, offline, or not resolvable from this machine)"
        )
    } else {
        format!("HTTP request to {url} failed: {detail}")
    }
}
static AGENT_CACHE: OnceLock<Mutex<AgentCache>> = OnceLock::new();
const MAX_CACHED_AGENTS: usize = 32;
#[derive(Default)]
struct AgentCache {
    agents: HashMap<String, ureq::Agent>,
    order: VecDeque<String>,
}
fn agent_cache_key(parts: &UrlParts, addrs: &[SocketAddr]) -> String {
    let mut ips: Vec<String> = addrs.iter().map(|addr| addr.ip().to_string()).collect();
    ips.sort();
    format!(
        "{}|{}:{}|{}",
        parts.scheme,
        parts.host,
        parts.port,
        ips.join(",")
    )
}
// SSRF 검사가 확인한 주소만 돌려주는 고정 resolver: 검증 시점과 연결 시점의 대상이 항상
// 일치한다. SNI·인증서 검증은 URI 호스트명으로 그대로 진행된다.
#[derive(Debug)]
struct PinnedResolver {
    addrs: Vec<SocketAddr>,
}
impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut out = self.empty();
        for addr in self.addrs.iter().take(16) {
            out.push(*addr);
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}
// 프로세스 공용 schannel 자격 증명
#[cfg(windows)]
static SCHANNEL_CRED: OnceLock<schannel::schannel_cred::SchannelCred> = OnceLock::new();
// https 전송을 schannel TLS로 감쌈 (OS 인증서 저장소·호스트명 검증)
#[cfg(windows)]
#[derive(Debug)]
struct SchannelConnector;
#[cfg(windows)]
impl<In: Transport> Connector<In> for SchannelConnector {
    type Out = Either<In, SchannelTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let host = details
            .uri
            .host()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let cred = match SCHANNEL_CRED.get() {
            Some(cred) => cred.clone(),
            None => {
                let acquired = schannel::schannel_cred::SchannelCred::builder()
                    .acquire(schannel::schannel_cred::Direction::Outbound)?;
                SCHANNEL_CRED.get_or_init(|| acquired).clone()
            }
        };
        let mut adapter = TransportAdapter::new(transport.boxed(), details.current_time.clone());
        adapter.set_timeout(details.timeout);
        let stream = schannel::tls_stream::Builder::new()
            .domain(&host)
            .connect(cred, adapter)
            .map_err(|error| match error {
                schannel::tls_stream::HandshakeError::Failure(error) => {
                    ureq::Error::Io(io::Error::new(
                        error.kind(),
                        format!("TLS handshake with {host} failed: {error}"),
                    ))
                }
                schannel::tls_stream::HandshakeError::Interrupted(_) => ureq::Error::Io(
                    io::Error::other(format!("TLS handshake with {host} was interrupted")),
                ),
            })?;
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(SchannelTransport { buffers, stream })))
    }
}
#[cfg(windows)]
struct SchannelTransport {
    buffers: LazyBuffers,
    stream: schannel::tls_stream::TlsStream<TransportAdapter>,
}
#[cfg(windows)]
impl std::fmt::Debug for SchannelTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SchannelTransport").finish()
    }
}
#[cfg(windows)]
impl Transport for SchannelTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        io::Write::write_all(&mut self.stream, &self.buffers.output()[..amount])?;
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let amount = io::Read::read(&mut self.stream, self.buffers.input_append_buf())?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }
    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }
    fn is_tls(&self) -> bool {
        true
    }
}
fn cached_agent(key: &str, pinned: &[SocketAddr]) -> ureq::Agent {
    let cache = AGENT_CACHE.get_or_init(|| Mutex::new(AgentCache::default()));
    let mut cache = cache.lock().unwrap();
    if let Some(agent) = cache.agents.get(key) {
        return agent.clone();
    }
    let config = ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    // Windows: OS TLS(schannel) 연결 체인 (ring·rustls 미사용)
    #[cfg(windows)]
    let connector =
        ().chain(ConnectProxyConnector::default())
            .chain(TcpConnector::default())
            .chain(SchannelConnector);
    #[cfg(not(windows))]
    let connector = DefaultConnector::default();
    // allow_private(검증 생략) 경로는 pinned가 비어 기본 resolver를 유지한다.
    let agent: ureq::Agent = if pinned.is_empty() {
        ureq::Agent::with_parts(config, connector, DefaultResolver::default())
    } else {
        ureq::Agent::with_parts(
            config,
            connector,
            PinnedResolver {
                addrs: pinned.to_vec(),
            },
        )
    };
    // FIFO 상한: 오래된 호스트 키부터 밀어내 무한 증식을 막는다(go maxTransports=32 동일).
    if cache.order.len() >= MAX_CACHED_AGENTS
        && let Some(evicted) = cache.order.pop_front()
    {
        cache.agents.remove(&evicted);
    }
    cache.agents.insert(key.to_string(), agent.clone());
    cache.order.push_back(key.to_string());
    agent
}
// 3. SSRF boundary ------------------------------------------------------------
pub fn ensure_url_allowed(url: &str, allow_private: bool) -> Result<(), String> {
    resolve_and_check(url, allow_private).map(|_| ())
}
// SSRF 검사와 함께 검증된 주소 집합을 돌려줘 Agent 캐시 키와 고정 resolver 재료로 쓴다.
fn resolve_and_check(
    url: &str,
    allow_private: bool,
) -> Result<(UrlParts, Vec<SocketAddr>), String> {
    let parts = parse_url(url)?;
    if allow_private {
        return Ok((parts, Vec::new()));
    }
    let addrs = (parts.host.as_str(), parts.port)
        .to_socket_addrs()
        .map_err(|error| format!("Failed to resolve host {}: {error}", parts.host))?;
    let mut sockets = Vec::new();
    for addr in addrs {
        if !is_allowed_ip(&addr.ip()) {
            return Err(format!(
                "Blocked non-public address {} for host {}",
                addr.ip(),
                parts.host
            ));
        }
        sockets.push(addr);
    }
    if sockets.is_empty() {
        return Err(format!(
            "Host {} did not resolve to any address",
            parts.host
        ));
    }
    Ok((parts, sockets))
}
// 로컬 개발 서버 접근을 위해 loopback(localhost/127.0.0.0/8/::1)은 허용한다.
// 그 외 사설·링크로컬·메타데이터 등 non-public 주소는 계속 차단한다.
fn is_allowed_ip(ip: &IpAddr) -> bool {
    is_public_ip(ip) || ip.is_loopback()
}
fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            let is_shared = octets[0] == 100 && (octets[1] & 0xc0) == 0x40; // 100.64.0.0/10 CGNAT
            let is_this_network = octets[0] == 0; // 0.0.0.0/8
            let is_multicast_or_reserved = octets[0] >= 224; // 224.0.0.0/4 + 240.0.0.0/4
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || is_shared
                || is_this_network
                || is_multicast_or_reserved)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // Canonicalize every IPv4-in-IPv6 embedding (mapped, compatible, NAT64, 6to4) and
            // re-check the embedded IPv4 so ::127.0.0.1 / 64:ff9b::7f00:1 / 2002:7f00:1:: cannot
            // smuggle a loopback/private target past the guard.
            if let Some(embedded) = embedded_ipv4(v6) {
                return is_public_ip(&IpAddr::V4(embedded));
            }
            let segments = v6.segments();
            let is_unique_local = (segments[0] & 0xfe00) == 0xfc00; // fc00::/7
            let is_link_local = (segments[0] & 0xffc0) == 0xfe80; // fe80::/10
            !(is_unique_local || is_link_local)
        }
    }
}
fn embedded_ipv4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    // IPv4-mapped ::ffff:a.b.c.d and deprecated IPv4-compatible ::a.b.c.d (high 96 bits zero),
    // plus NAT64 well-known prefix 64:ff9b::/96 — all carry the IPv4 in the low 32 bits.
    let low_prefix = seg[0..5].iter().all(|&word| word == 0) && (seg[5] == 0 || seg[5] == 0xffff);
    let nat64 = seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|&word| word == 0);
    if low_prefix || nat64 {
        return Some(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        ));
    }
    // 6to4 2002::/16 carries the IPv4 in the second and third 16-bit groups.
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            (seg[1] & 0xff) as u8,
            (seg[2] >> 8) as u8,
            (seg[2] & 0xff) as u8,
        ));
    }
    None
}
// 4. URL parsing / resolution -------------------------------------------------
struct UrlParts {
    scheme: String,
    host: String,
    port: u16,
}
fn parse_url(url: &str) -> Result<UrlParts, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "URL must include an http:// or https:// scheme".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("Only http:// and https:// URL schemes are accepted".to_string());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Drop any userinfo before the host.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (addr, tail) = stripped
            .split_once(']')
            .ok_or_else(|| "Invalid IPv6 authority: missing ']'".to_string())?;
        let port = match tail.strip_prefix(':') {
            Some(text) if !text.is_empty() => text
                .parse::<u16>()
                .map_err(|error| format!("Invalid port: {error}"))?,
            _ => default_port,
        };
        (addr.to_string(), port)
    } else if let Some((host, port_text)) = authority.rsplit_once(':') {
        if !port_text.is_empty() && port_text.chars().all(|ch| ch.is_ascii_digit()) {
            let port = port_text
                .parse::<u16>()
                .map_err(|error| format!("Invalid port: {error}"))?;
            (host.to_string(), port)
        } else {
            (authority.to_string(), default_port)
        }
    } else {
        (authority.to_string(), default_port)
    };
    if host.is_empty() {
        return Err("URL host is required".to_string());
    }
    Ok(UrlParts { scheme, host, port })
}
// Resolve a possibly-relative target (redirect Location or <a href>) against a base URL.
pub fn resolve_url(base: &str, target: &str) -> String {
    let target = target.trim();

    // 자체 scheme 보유 참조
    let scheme_len = target
        .bytes()
        .position(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))
        .unwrap_or(target.len());
    let has_scheme = target.as_bytes().get(scheme_len) == Some(&b':')
        && target.as_bytes()[0].is_ascii_alphabetic();
    if has_scheme {
        return target.to_string();
    }
    let Ok(parts) = parse_url(base) else {
        return target.to_string();
    };

    // 네트워크 경로 참조
    if let Some(rest) = target.strip_prefix("//") {
        return format!("{}://{}", parts.scheme, rest);
    }

    // base 권한·경로·쿼리
    let default_port = if parts.scheme == "https" { 443 } else { 80 };
    let host = if parts.host.contains(':') {
        format!("[{}]", parts.host)
    } else {
        parts.host.clone()
    };
    let authority = if parts.port == default_port {
        host
    } else {
        format!("{host}:{}", parts.port)
    };
    let after_scheme = base.split_once("://").map_or("", |(_, rest)| rest);
    let base_tail = after_scheme
        .find(['/', '?', '#'])
        .map_or("", |index| &after_scheme[index..]);
    let base_tail = base_tail.split('#').next().unwrap_or(base_tail);
    let (base_path, base_query) = match base_tail.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (base_tail, None),
    };
    let base_path = if base_path.is_empty() { "/" } else { base_path };

    // 참조 경로·쿼리·프래그먼트
    let (reference, fragment) = match target.split_once('#') {
        Some((reference, fragment)) => (reference, Some(fragment)),
        None => (target, None),
    };
    let (ref_path, ref_query) = match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    };

    // 경로 병합·점 세그먼트 제거 (RFC 3986 5.2)
    let (path, query) = if ref_path.is_empty() {
        (base_path.to_string(), ref_query.or(base_query))
    } else {
        let merged = if ref_path.starts_with('/') {
            ref_path.to_string()
        } else {
            let dir_end = base_path.rfind('/').map_or(0, |index| index + 1);
            format!("{}{ref_path}", &base_path[..dir_end])
        };
        let mut segments: Vec<&str> = Vec::new();
        let mut pending = merged.split('/').skip(1).peekable();
        while let Some(segment) = pending.next() {
            let dot = matches!(segment, "." | "..");
            if segment == ".." {
                segments.pop();
            }
            if !dot {
                segments.push(segment);
            } else if pending.peek().is_none() {
                segments.push("");
            }
        }
        (format!("/{}", segments.join("/")), ref_query)
    };
    let mut resolved = format!("{}://{authority}{path}", parts.scheme);
    if let Some(query) = query {
        resolved.push('?');
        resolved.push_str(query);
    }
    if let Some(fragment) = fragment {
        resolved.push('#');
        resolved.push_str(fragment);
    }
    resolved
}
// 4b. Body decoding / limit errors --------------------------------------------
// charset=euc-kr 같은 비UTF-8 본문을 lossy UTF-8로 읽으면 전부 U+FFFD로 깨진다.
// Content-Type 헤더 → HTML meta(앞 2048바이트) 순으로 라벨을 찾아 encoding_rs로 디코딩한다.
pub fn decode_body(content_type: &str, body: &[u8]) -> String {
    let label = charset_after(content_type).or_else(|| {
        // HTML5는 meta charset 선언을 문서 앞 1024바이트 안에 두라고 요구한다.
        let head = &body[..body.len().min(2048)];
        charset_after(&String::from_utf8_lossy(head))
    });
    if let Some(label) = label
        && let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes())
        && encoding != encoding_rs::UTF_8
    {
        return encoding.decode(body).0.into_owned();
    }
    String::from_utf8_lossy(body).into_owned()
}
fn charset_after(text: &str) -> Option<String> {
    let lowered = text.to_ascii_lowercase();
    let index = lowered.find("charset=")?;
    let tail = lowered[index + 8..].trim_start_matches(['"', '\'', ' ']);
    let end = tail
        .find([';', '"', '\'', ' ', '>', '/'])
        .unwrap_or(tail.len());
    let value = tail[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}
// limit 초과는 maxBytes 증액이 해법임을 에러 문장에 직접 싣는다(에이전트 재시도 유도).
fn body_limit_error(max_bytes: u64, detail: &str) -> String {
    // ureq는 "larger than request limit", writer 경로는 "body exceeds limit"을 쓴다.
    let lowered = detail.to_ascii_lowercase();
    if lowered.contains("exceed") || lowered.contains("larger than") {
        format!("Response body exceeds the {max_bytes}-byte limit; retry with a larger maxBytes")
    } else {
        format!("Failed to read response body (limit {max_bytes} bytes): {detail}")
    }
}
// 5. Dump modes ---------------------------------------------------------------
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DumpMode {
    Html,
    Text,
    Markdown,
    Links,
    Readability,
}
pub fn parse_dump(value: &str) -> Result<DumpMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "html" => Ok(DumpMode::Html),
        "text" | "txt" => Ok(DumpMode::Text),
        "markdown" | "md" => Ok(DumpMode::Markdown),
        "links" => Ok(DumpMode::Links),
        "readability" | "article" | "readable" => Ok(DumpMode::Readability),
        other => Err(format!(
            "Unknown dump mode '{other}' (use html|text|markdown|links|readability)"
        )),
    }
}
// Render an HTML document into the requested dump form. base resolves relative links.
pub fn render_html(mode: DumpMode, html: &str, base: Option<&str>) -> Result<String, String> {
    match mode {
        DumpMode::Html => Ok(html.to_string()),
        DumpMode::Text => Ok(html_to_text(html)),
        DumpMode::Markdown => Ok(html_to_markdown(html)),
        DumpMode::Links => Ok(html_links(html, base).join("\n")),
        DumpMode::Readability => {
            let doc = html_readability(html);
            Ok(if doc.title.is_empty() {
                doc.text
            } else {
                format!("# {}\n\n{}", doc.title, doc.text)
            })
        }
    }
}
// 6. Test fixtures ------------------------------------------------------------
// 응답 후 연결 유지 (127.0.0.1 리셋 회피)
#[cfg(test)]
pub(crate) fn serve_loopback_http<F>(
    resets: usize,
    respond: F,
) -> (String, std::thread::JoinHandle<usize>)
where
    F: FnOnce(&str) -> String + Send + 'static,
{
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        // 연결 대기 상한 (실패 테스트 hang 방지)
        let accept_within_deadline = || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                if let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    return Some(stream);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        };
        // 요청 미수신 종료 (RST 유발)
        for accepted in 0..resets {
            let Some(stream) = accept_within_deadline() else {
                return accepted;
            };
            let mut probe = [0u8; 1];
            let _ = stream.peek(&mut probe);
        }
        let Some(mut stream) = accept_within_deadline() else {
            return resets;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let response = respond(&String::from_utf8_lossy(&request));
        let _ = stream.write_all(response.as_bytes());
        // 종료 미전달 연결 대기 상한
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        while matches!(stream.read(&mut buffer), Ok(read) if read > 0) {}
        resets + 1
    });
    (url, server)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn blocks_private_hosts() {
        for url in [
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            assert!(
                ensure_url_allowed(url, false).is_err(),
                "{url} should be blocked"
            );
        }
    }
    #[test]
    fn allows_loopback_hosts() {
        // localhost(loopback) 대상은 로컬 개발 서버 접근을 위해 SSRF guard를 통과한다.
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/",
            "http://[::1]/",
            "http://localhost/",
        ] {
            assert!(
                ensure_url_allowed(url, false).is_ok(),
                "{url} should be allowed"
            );
        }
    }
    #[test]
    fn allow_private_flag_bypasses_guard() {
        assert!(ensure_url_allowed("http://127.0.0.1/", true).is_ok());
    }
    // 403 회피용 기본 헤더 ----------------------------------------------------
    #[test]
    fn default_headers_match_browser_gated_servers() {
        let (url, server) = serve_loopback_http(0, |request| {
            let request = request.to_ascii_lowercase();
            let allowed = request.contains("user-agent: mozilla/5.0")
                && request.contains("accept: text/html,application/xhtml+xml")
                && request.contains("accept-language: en-us,en;q=0.9")
                && request.contains("upgrade-insecure-requests: 1")
                && request.contains("sec-fetch-mode: navigate")
                && request.contains("sec-ch-ua-platform:");
            let (status, body) = if allowed {
                ("200 OK", "ok")
            } else {
                ("403 Forbidden", "blocked")
            };
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        });

        let page = http_fetch(&url, &FetchOptions::default(), false).unwrap();

        server.join().unwrap();
        assert_eq!(page.status, 200);
        assert_eq!(page.body_text(), "ok");
    }
    #[test]
    fn rejects_non_http_schemes() {
        assert!(ensure_url_allowed("file:///etc/passwd", false).is_err());
        assert!(ensure_url_allowed("ftp://example.com/", false).is_err());
    }
    #[test]
    fn classifies_public_vs_private_ips() {
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_public_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        // Multicast (224/4) and reserved class-E (240/4) are non-public.
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(!is_public_ip(&IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1))));
    }
    #[test]
    fn allowed_ip_permits_loopback_only_among_private() {
        // is_allowed_ip = public 또는 loopback. loopback만 예외이고 나머지 사설은 여전히 거부한다.
        assert!(is_allowed_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_allowed_ip(&IpAddr::V4(Ipv4Addr::new(127, 9, 9, 9))));
        assert!(is_allowed_ip(&"::1".parse().unwrap()));
        assert!(is_allowed_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_allowed_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_allowed_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        // IPv4-mapped loopback 같은 내장 형태는 예외로 인정하지 않아 계속 차단된다.
        assert!(!is_allowed_ip(&"::ffff:127.0.0.1".parse().unwrap()));
    }
    #[test]
    fn blocks_ipv4_embedded_in_ipv6() {
        // IPv4-in-IPv6 forms that resolve to loopback/private/metadata must be rejected.
        for url in [
            "http://[::127.0.0.1]/",            // IPv4-compatible
            "http://[::ffff:127.0.0.1]/",       // IPv4-mapped
            "http://[::ffff:169.254.169.254]/", // mapped metadata
            "http://[64:ff9b::7f00:1]/",        // NAT64 -> 127.0.0.1
            "http://[2002:7f00:1::]/",          // 6to4 -> 127.0.0.1
            "http://[2002:a00:1::]/",           // 6to4 -> 10.0.0.1
        ] {
            assert!(
                ensure_url_allowed(url, false).is_err(),
                "{url} should be blocked"
            );
        }
        // A genuine global IPv6 stays allowed (subject to DNS-free literal check).
        assert!(is_public_ip(&"2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn resolves_relative_and_root_urls() {
        assert_eq!(resolve_url("https://ex.com/a/b", "/c"), "https://ex.com/c");
        assert_eq!(resolve_url("https://ex.com/a/b", "c"), "https://ex.com/a/c");
        assert_eq!(
            resolve_url("https://ex.com/a/b", "https://other.com/x"),
            "https://other.com/x"
        );
        assert_eq!(
            resolve_url("https://ex.com/a", "//cdn.com/x"),
            "https://cdn.com/x"
        );
    }
    #[test]
    fn resolves_rfc3986_reference_examples() {
        // RFC 3986 5.4 예제
        let base = "http://a/b/c/d;p?q";
        for (reference, expected) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            (";x", "http://a/b/c/;x"),
            ("g;x?y#s", "http://a/b/c/g;x?y#s"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            (".g", "http://a/b/c/.g"),
            ("g..", "http://a/b/c/g.."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("./g/.", "http://a/b/c/g/"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
            ("g;x=1/../y", "http://a/b/c/y"),
            ("g?y/./x", "http://a/b/c/g?y/./x"),
            ("g#s/./x", "http://a/b/c/g#s/./x"),
        ] {
            assert_eq!(resolve_url(base, reference), expected, "{reference}");
        }
    }
    #[test]
    fn resolves_scheme_and_base_edge_cases() {
        // 상대 참조 해석
        for (base, reference, expected) in [
            ("https://h/a/list", "?page=2", "https://h/a/list?page=2"),
            ("https://h/a/list?q=1#top", "#x", "https://h/a/list?q=1#x"),
            ("https://h", "?page=2", "https://h/?page=2"),
            ("https://h?old=1", "p", "https://h/p"),
            ("https://h/a/list", "./a:b", "https://h/a/a:b"),
            ("https://h/a/list", "1x:y", "https://h/a/1x:y"),
            (
                "http://127.0.0.1:8080/a/b",
                "../c?d#e",
                "http://127.0.0.1:8080/c?d#e",
            ),
            ("http://[::1]:8080/a", "/b", "http://[::1]:8080/b"),
            ("https://ex.com:443/a", "b", "https://ex.com/b"),
        ] {
            assert_eq!(resolve_url(base, reference), expected, "{reference}");
        }
        // 자체 scheme 참조 원문 유지
        for target in [
            "HTTPS://X/",
            "Http://x/y",
            "data:text/plain,hi",
            "ftp://f/x",
            "mailto:x@example.com",
            "javascript:void(0)",
            "tel:+1",
        ] {
            assert_eq!(resolve_url("https://h/a/list", target), target);
        }
    }
    #[test]
    fn redirect_hops_stay_guarded() {
        for (location, expected) in [
            (
                "HTTP://169.254.169.254/latest/",
                "Blocked non-public address",
            ),
            ("//10.0.0.1/admin", "Blocked non-public address"),
            ("ftp://example.com/file", "Only http:// and https://"),
            (
                "javascript:alert(1)",
                "must include an http:// or https:// scheme",
            ),
        ] {
            let (url, server) = serve_loopback_http(0, move |_| {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            });
            let error = http_fetch(&url, &FetchOptions::default(), false).unwrap_err();
            server.join().unwrap();
            assert!(error.contains(expected), "{location}: {error}");
        }
    }
    #[test]
    fn renders_deep_nesting_in_every_mode() {
        let html = format!(
            "<p><a href=\"/x\">deep</a></p>{}tail",
            "<div>".repeat(100_000)
        );
        for (mode, expected) in [
            (DumpMode::Html, html.as_str()),
            (DumpMode::Text, "deep\n\ntail"),
            (DumpMode::Markdown, "[deep](/x)\n\ntail"),
            (DumpMode::Links, "https://ex.com/x"),
            (DumpMode::Readability, "deep\n\ntail"),
        ] {
            let rendered = render_html(mode, &html, Some("https://ex.com/")).unwrap();
            assert_eq!(rendered, expected, "{mode:?}");
        }
    }
    #[test]
    fn renders_dump_modes() {
        let html = "<html><head><title>T</title></head><body><h1>Title</h1><p>Body <a href=\"/x\">x</a></p></body></html>";
        assert_eq!(
            render_html(DumpMode::Text, html, None).unwrap(),
            "Title\n\nBody x"
        );
        assert_eq!(
            render_html(DumpMode::Links, html, Some("https://ex.com/")).unwrap(),
            "https://ex.com/x"
        );
        assert!(
            render_html(DumpMode::Readability, html, None)
                .unwrap()
                .starts_with("# T")
        );
    }
    #[test]
    fn parses_dump_modes() {
        assert_eq!(parse_dump("MD").unwrap(), DumpMode::Markdown);
        assert_eq!(parse_dump("links").unwrap(), DumpMode::Links);
        assert!(parse_dump("bogus").is_err());
    }
    #[test]
    fn decodes_euc_kr_body_via_content_type() {
        // EUC-KR "한글"(C7D1 B1DB)이 lossy UTF-8 경로로 깨지지 않아야 한다.
        let page = FetchedPage {
            status: 200,
            content_type: "text/html; charset=euc-kr".to_string(),
            body: vec![0xC7, 0xD1, 0xB1, 0xDB],
            final_url: "http://example.com/".to_string(),
        };
        assert_eq!(page.body_text(), "한글");
    }
    #[test]
    fn decodes_charset_from_meta_tag() {
        // 헤더에 charset이 없으면 문서 앞부분의 meta 선언으로 폴백한다.
        let mut body = b"<html><head><meta charset=\"euc-kr\"></head><body>".to_vec();
        body.extend([0xC7, 0xD1, 0xB1, 0xDB]);
        body.extend_from_slice(b"</body></html>");
        let page = FetchedPage {
            status: 200,
            content_type: "text/html".to_string(),
            body,
            final_url: String::new(),
        };
        assert!(page.body_text().contains("한글"), "{}", page.body_text());
    }
    #[test]
    fn body_limit_error_hints_max_bytes() {
        // limit 초과 응답은 maxBytes 증액 힌트를 담은 에러로 끝나야 한다.
        let (url, server) = serve_loopback_http(0, |_| {
            let body = "x".repeat(4096);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        });
        let opts = FetchOptions {
            max_bytes: 1024,
            ..FetchOptions::default()
        };
        let error = http_fetch(&url, &opts, false).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("maxBytes"), "{error}");
    }
    // 응답 전 연결 리셋 재시도 ------------------------------------------------
    #[test]
    fn retries_get_after_reset_before_response() {
        // 리셋 1회 + 성공 1회
        let (url, server) = serve_loopback_http(1, |_| {
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string()
        });
        let page = http_fetch(&url, &FetchOptions::default(), false).unwrap();
        assert_eq!(page.body_text(), "hello");
        assert_eq!(server.join().unwrap(), 2);
    }
    #[test]
    fn retried_download_writes_body_once() {
        let (url, server) = serve_loopback_http(1, |_| {
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string()
        });
        let mut written = Vec::new();
        let meta =
            http_fetch_to_writer(&url, &FetchOptions::default(), false, &mut written).unwrap();
        assert_eq!(meta.bytes, 5);
        assert_eq!(written, b"hello");
        assert_eq!(server.join().unwrap(), 2);
    }
    #[test]
    fn reset_retries_stay_bounded() {
        // 리셋 2회: 3번째 시도 없음
        let (url, server) = serve_loopback_http(2, |_| {
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string()
        });
        let error = http_fetch(&url, &FetchOptions::default(), false).unwrap_err();
        assert!(error.starts_with("HTTP request to "), "{error}");
        assert!(
            error.contains("connection reset before any response on both attempts")
                && error.contains("retried once"),
            "{error}"
        );
        // 응답 대기 서버 해제 연결 (집계 +1)
        let _ =
            std::net::TcpStream::connect(url.trim_start_matches("http://").trim_end_matches('/'));
        assert_eq!(server.join().unwrap(), 3);
    }
    // 최대 timeoutMs ----------------------------------------------------------
    #[test]
    fn max_timeout_ms_does_not_overflow_deadline() {
        let (url, server) = serve_loopback_http(0, |_| {
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_string()
        });
        let opts = FetchOptions {
            timeout_ms: u64::MAX,
            ..FetchOptions::default()
        };
        let page = http_fetch(&url, &opts, false).unwrap();
        assert_eq!(page.body_text(), "hello");
        server.join().unwrap();
    }
}
