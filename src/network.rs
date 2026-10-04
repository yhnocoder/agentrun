use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::net::{
    Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs,
};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::cli::{NetworkMode, Runtime};
use crate::event::{Network, NetworkReason};

pub const SOCKET_FILE: &str = "proxy.sock";
pub const PORT_PLACEHOLDER: &str = "<proxy port>";
pub const PROXY_VARIABLES: [&str; 6] = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
];
pub const NO_PROXY_VARIABLES: [&str; 2] = ["NO_PROXY", "no_proxy"];
const SECURE_UPSTREAM_VARIABLES: [&str; 4] =
    ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
const PLAIN_UPSTREAM_VARIABLES: [&str; 4] = ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"];
const LISTEN_ADDRESS: Ipv4Addr = Ipv4Addr::LOCALHOST;
const DEFAULT_PORTS: [u16; 2] = [443, 80];
const SERVICE_PORT: u16 = 443;
const HTTP_DEFAULT_PORT: u16 = 80;
const HEAD_LIMIT: usize = 64 * 1024;
const HOST_NAME_LIMIT: usize = 253;
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PRIVATE_RANGES: [(Ipv4Addr, u8); 7] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
];
const SOCKS_VERSION: u8 = 0x05;
const SOCKS_NO_AUTH: u8 = 0x00;
const SOCKS_NO_ACCEPTABLE_METHOD: u8 = 0xff;
const SOCKS_CONNECT: u8 = 0x01;
const SOCKS_ATYP_IPV4: u8 = 0x01;
const SOCKS_ATYP_DOMAIN: u8 = 0x03;
const SOCKS_SUCCEEDED: u8 = 0x00;
const SOCKS_NOT_ALLOWED: u8 = 0x02;
const SOCKS_HOST_UNREACHABLE: u8 = 0x04;
const SOCKS_CONNECTION_REFUSED: u8 = 0x05;
const SOCKS_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const SOCKS_ADDRESS_NOT_SUPPORTED: u8 = 0x08;

pub fn proxy_needed(runtime: Runtime, mode: NetworkMode, sandboxed: bool) -> bool {
    sandboxed
        && match runtime {
            Runtime::Pi => true,
            Runtime::ClaudeCode => mode != NetworkMode::None,
            Runtime::Codex => false,
        }
}

pub fn proxy_environment(port: &str) -> Vec<(OsString, OsString)> {
    let address = format!("http://{LISTEN_ADDRESS}:{port}");
    PROXY_VARIABLES
        .iter()
        .map(|name| (OsString::from(name), OsString::from(&address)))
        .chain(
            NO_PROXY_VARIABLES
                .iter()
                .map(|name| (OsString::from(name), OsString::new())),
        )
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRule {
    pub host: String,
    pub wildcard: bool,
    pub port: Option<u16>,
}

impl HostRule {
    pub fn matches(&self, host: &str, port: u16) -> bool {
        let host_matches = if self.wildcard {
            host.len() > self.host.len() + 1
                && host.ends_with(&self.host)
                && host.as_bytes()[host.len() - self.host.len() - 1] == b'.'
        } else {
            host == self.host
        };
        host_matches
            && match self.port {
                Some(rule_port) => rule_port == port,
                None => DEFAULT_PORTS.contains(&port),
            }
    }

    pub fn pattern(&self) -> String {
        if self.wildcard {
            format!("*.{}", self.host)
        } else {
            self.host.clone()
        }
    }
}

pub fn check_usage(mode: NetworkMode, allow_host: &[String]) -> Result<Vec<HostRule>, String> {
    if mode == NetworkMode::Custom && allow_host.is_empty() {
        return Err("--network custom requires at least one --allow-host".to_string());
    }
    if mode != NetworkMode::Custom && !allow_host.is_empty() {
        return Err("--allow-host requires --network custom".to_string());
    }
    allow_host
        .iter()
        .map(|value| parse_allow_host(value))
        .collect()
}

fn parse_allow_host(value: &str) -> Result<HostRule, String> {
    if value == "*" {
        return Err("--allow-host '*' is not allowed. Use --network full".to_string());
    }
    let malformed = || format!("--allow-host '{value}': expected HOST or HOST:PORT");
    let ipv6 = || format!("--allow-host '{value}': IPv6 addresses are not supported");
    if value.starts_with('[') {
        return Err(ipv6());
    }
    let (host, port) = match value.rsplit_once(':') {
        Some((host, _)) if host.contains(':') => return Err(ipv6()),
        Some((host, port)) => (host, Some(port)),
        None => (value, None),
    };
    let port = match port {
        Some(port) => Some(
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(malformed)?,
        ),
        None => None,
    };
    let host = host.to_lowercase();
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return Err(malformed());
    }
    let (host, wildcard) = match host.strip_prefix("*.") {
        Some(rest) => (rest, true),
        None => (host, false),
    };
    if host.is_empty() {
        return Err(malformed());
    }
    if host.contains('*') {
        return Err(format!(
            "--allow-host '{value}': a wildcard is only allowed as the first label, as in *.example.com"
        ));
    }
    if !valid_host_name(host) {
        return Err(malformed());
    }
    Ok(HostRule {
        host: host.to_string(),
        wildcard,
        port,
    })
}

pub fn is_private(address: Ipv4Addr) -> bool {
    PRIVATE_RANGES
        .iter()
        .any(|(network, bits)| in_cidr(address, *network, *bits))
}

fn in_cidr(address: Ipv4Addr, network: Ipv4Addr, bits: u8) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(bits))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub mode: NetworkMode,
    pub rules: Vec<HostRule>,
    pub service_hosts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Allowance {
    Listed,
    Service,
    Any,
}

impl Policy {
    fn allowance(&self, host: &str, port: u16) -> Option<Allowance> {
        let service = || {
            (port == SERVICE_PORT && self.service_hosts.iter().any(|service| service == host))
                .then_some(Allowance::Service)
        };
        match self.mode {
            NetworkMode::Full => Some(Allowance::Any),
            NetworkMode::Custom => {
                let matching: Vec<&HostRule> = self
                    .rules
                    .iter()
                    .filter(|rule| rule.matches(host, port))
                    .collect();
                if matching.iter().any(|rule| !rule.wildcard) {
                    Some(Allowance::Listed)
                } else if !matching.is_empty() {
                    Some(Allowance::Any)
                } else {
                    service()
                }
            }
            NetworkMode::None => service(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyAddress {
    pub host: String,
    pub port: u16,
    pub authorization: Option<String>,
}

impl ProxyAddress {
    fn parse(value: &str) -> Option<ProxyAddress> {
        let rest = match value.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some(_) => return None,
            None => value,
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (userinfo, host_port) = match authority.rsplit_once('@') {
            Some((userinfo, host_port)) => (Some(userinfo), host_port),
            None => (None, authority),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => (host, port.parse::<u16>().ok()?),
            _ => (host_port, HTTP_DEFAULT_PORT),
        };
        if host.is_empty() {
            return None;
        }
        let authorization = userinfo.map(|userinfo| {
            let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            let pair = format!("{}:{}", percent_decode(user), percent_decode(password));
            format!("Basic {}", base64(pair.as_bytes()))
        });
        Some(ProxyAddress {
            host: host.to_string(),
            port,
            authorization,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoProxyRule {
    All,
    Host(String),
    Suffix(String),
    Cidr(Ipv4Addr, u8),
}

impl NoProxyRule {
    fn parse(item: &str) -> Option<NoProxyRule> {
        let item = item.trim().to_lowercase();
        if item.is_empty() {
            return None;
        }
        if item == "*" {
            return Some(NoProxyRule::All);
        }
        if let Some((network, bits)) = item.split_once('/') {
            let network = network.parse::<Ipv4Addr>().ok()?;
            let bits = bits.parse::<u8>().ok().filter(|bits| *bits <= 32)?;
            return Some(NoProxyRule::Cidr(network, bits));
        }
        let item = match item.rsplit_once(':') {
            Some((host, port))
                if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) =>
            {
                host.to_string()
            }
            _ => item,
        };
        if let Some(suffix) = item.strip_prefix("*.") {
            return Some(NoProxyRule::Suffix(
                suffix.trim_start_matches('.').to_string(),
            ));
        }
        if let Some(suffix) = item.strip_prefix('.') {
            return Some(NoProxyRule::Suffix(suffix.to_string()));
        }
        Some(NoProxyRule::Host(item))
    }

    fn matches(&self, host: &str, addresses: &[Ipv4Addr]) -> bool {
        match self {
            NoProxyRule::All => true,
            NoProxyRule::Host(name) => host == name,
            NoProxyRule::Suffix(suffix) => {
                host == suffix
                    || (host.len() > suffix.len()
                        && host.ends_with(suffix)
                        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.')
            }
            NoProxyRule::Cidr(network, bits) => host
                .parse::<Ipv4Addr>()
                .ok()
                .into_iter()
                .chain(addresses.iter().copied())
                .any(|address| in_cidr(address, *network, *bits)),
        }
    }
}

pub fn parse_no_proxy(value: &str) -> Vec<NoProxyRule> {
    value.split(',').filter_map(NoProxyRule::parse).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Upstream {
    pub secure: Option<ProxyAddress>,
    pub plain: Option<ProxyAddress>,
    pub no_proxy: Vec<NoProxyRule>,
}

impl Upstream {
    pub fn from_env(env: &BTreeMap<OsString, OsString>) -> (Upstream, Vec<String>) {
        let mut notes = Vec::new();
        let value = |name: &str| {
            env.get(OsStr::new(name))
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
        };
        let mut pick = |names: &[&str]| {
            let (name, found) = names
                .iter()
                .find_map(|name| value(name).map(|found| (*name, found)))?;
            let parsed = ProxyAddress::parse(found);
            if parsed.is_none() {
                notes.push(format!(
                    "upstream proxy ignored: {name} uses an unsupported scheme"
                ));
            }
            parsed
        };
        let secure = pick(&SECURE_UPSTREAM_VARIABLES);
        let plain = pick(&PLAIN_UPSTREAM_VARIABLES);
        let no_proxy = NO_PROXY_VARIABLES
            .iter()
            .rev()
            .find_map(|name| value(name))
            .map(parse_no_proxy)
            .unwrap_or_default();
        (
            Upstream {
                secure,
                plain,
                no_proxy,
            },
            notes,
        )
    }

    fn select(&self, secure: bool, host: &str, addresses: &[Ipv4Addr]) -> Option<&ProxyAddress> {
        let candidate = if secure { &self.secure } else { &self.plain };
        candidate.as_ref().filter(|_| {
            !self
                .no_proxy
                .iter()
                .any(|rule| rule.matches(host, addresses))
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub port: Option<u16>,
    pub socket: PathBuf,
}

impl ProxyEndpoint {
    pub fn port_text(&self) -> String {
        match self.port {
            Some(port) => port.to_string(),
            None => PORT_PLACEHOLDER.to_string(),
        }
    }
}

type Report = Box<dyn Fn(Network) + Send + Sync>;

struct Server {
    policy: Policy,
    upstream: Upstream,
    report: Report,
    seen: Mutex<HashSet<(String, u16, bool)>>,
}

pub struct FilterProxy {
    port: u16,
    socket: PathBuf,
    tcp: Option<TcpListener>,
    unix: Option<UnixListener>,
    stopping: Arc<AtomicBool>,
    acceptors: Vec<JoinHandle<()>>,
}

impl FilterProxy {
    pub fn bind(tempdir: &Path) -> io::Result<FilterProxy> {
        let tcp = TcpListener::bind(SocketAddrV4::new(LISTEN_ADDRESS, 0))?;
        let port = tcp.local_addr()?.port();
        let socket = tempdir.join(SOCKET_FILE);
        let unix = UnixListener::bind(&socket)?;
        tcp.set_nonblocking(true)?;
        unix.set_nonblocking(true)?;
        Ok(FilterProxy {
            port,
            socket,
            tcp: Some(tcp),
            unix: Some(unix),
            stopping: Arc::new(AtomicBool::new(false)),
            acceptors: Vec::new(),
        })
    }

    pub fn endpoint(&self) -> ProxyEndpoint {
        ProxyEndpoint {
            port: Some(self.port),
            socket: self.socket.clone(),
        }
    }

    pub fn serve(
        &mut self,
        policy: Policy,
        upstream: Upstream,
        report: impl Fn(Network) + Send + Sync + 'static,
    ) {
        let server = Arc::new(Server {
            policy,
            upstream,
            report: Box::new(report),
            seen: Mutex::new(HashSet::new()),
        });
        if let Some(tcp) = self.tcp.take() {
            let server = Arc::clone(&server);
            let stopping = Arc::clone(&self.stopping);
            self.acceptors.push(thread::spawn(move || {
                accept_until_stopped(tcp.as_raw_fd(), &stopping, &server, || {
                    tcp.accept().ok().map(|(stream, _)| Client::Tcp(stream))
                });
            }));
        }
        if let Some(unix) = self.unix.take() {
            let server = Arc::clone(&server);
            let stopping = Arc::clone(&self.stopping);
            self.acceptors.push(thread::spawn(move || {
                accept_until_stopped(unix.as_raw_fd(), &stopping, &server, || {
                    unix.accept().ok().map(|(stream, _)| Client::Unix(stream))
                });
            }));
        }
    }
}

fn accept_until_stopped(
    fd: RawFd,
    stopping: &AtomicBool,
    server: &Arc<Server>,
    mut accept: impl FnMut() -> Option<Client>,
) {
    while !stopping.load(Ordering::SeqCst) {
        if !readable_within(fd, STOP_POLL_INTERVAL) {
            continue;
        }
        if let Some(client) = accept() {
            let _ = client.set_nonblocking(false);
            let server = Arc::clone(server);
            thread::spawn(move || handle(client, &server));
        }
    }
}

fn readable_within(fd: RawFd, timeout: Duration) -> bool {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout.as_millis() as libc::c_int) };
    ready > 0
}

impl Drop for FilterProxy {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        for acceptor in self.acceptors.drain(..) {
            let _ = acceptor.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

enum Client {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Client {
    fn try_clone(&self) -> io::Result<Client> {
        Ok(match self {
            Client::Tcp(stream) => Client::Tcp(stream.try_clone()?),
            Client::Unix(stream) => Client::Unix(stream.try_clone()?),
        })
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Client::Tcp(stream) => stream.set_read_timeout(timeout),
            Client::Unix(stream) => stream.set_read_timeout(timeout),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Client::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Client::Unix(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Client::Tcp(stream) => stream.shutdown(how),
            Client::Unix(stream) => stream.shutdown(how),
        }
    }
}

impl Read for Client {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Client::Tcp(stream) => stream.read(buffer),
            Client::Unix(stream) => stream.read(buffer),
        }
    }
}

impl Write for Client {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Client::Tcp(stream) => stream.write(buffer),
            Client::Unix(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Client::Tcp(stream) => stream.flush(),
            Client::Unix(stream) => stream.flush(),
        }
    }
}

struct Target {
    host: String,
    port: u16,
}

enum Route {
    Direct(Vec<SocketAddrV4>),
    Upstream(ProxyAddress),
}

fn handle(mut client: Client, server: &Server) {
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    let mut first = [0u8; 1];
    if client.read(&mut first).ok() != Some(1) {
        return;
    }
    if first[0] == SOCKS_VERSION {
        handle_socks(client, server);
    } else {
        handle_http(client, server, first[0]);
    }
}

fn handle_http(mut client: Client, server: &Server, first: u8) {
    let Some((head, leftover)) = read_head(&mut client, vec![first]) else {
        return;
    };
    let _ = client.set_read_timeout(None);
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let headers: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        respond(&mut client, "400 Bad Request", "");
        return;
    };
    if method.eq_ignore_ascii_case("CONNECT") {
        let Some(target) = parse_host_port(target, None) else {
            respond(&mut client, "400 Bad Request", "");
            return;
        };
        let route = match admit(server, &target, true) {
            Err(reason) => {
                respond(&mut client, "403 Forbidden", &denied_body(&target, reason));
                return;
            }
            Ok(route) => route,
        };
        match route.and_then(|route| connect(&route, &target, true)) {
            None => respond(&mut client, "502 Bad Gateway", ""),
            Some((stream, early)) => {
                if write_all(&mut client, b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    && write_all(&mut client, &early)
                {
                    relay(client, stream, &leftover);
                }
            }
        }
        return;
    }
    let Some((authority, path)) = split_absolute_url(target) else {
        respond(&mut client, "400 Bad Request", "");
        return;
    };
    let Some(target) = parse_host_port(&authority, Some(HTTP_DEFAULT_PORT)) else {
        respond(&mut client, "400 Bad Request", "");
        return;
    };
    if !version.starts_with("HTTP/1.") {
        respond(&mut client, "400 Bad Request", "");
        return;
    }
    let body_length = match request_body_length(&headers) {
        Ok(length) => length,
        Err(status) => {
            respond(&mut client, status, "");
            return;
        }
    };
    let route = match admit(server, &target, false) {
        Err(reason) => {
            respond(&mut client, "403 Forbidden", &denied_body(&target, reason));
            return;
        }
        Ok(Some(route)) => route,
        Ok(None) => {
            respond(&mut client, "502 Bad Gateway", "");
            return;
        }
    };
    let authority = authority_text(&target);
    let request_target = match &route {
        Route::Upstream(_) => format!("http://{authority}{path}"),
        Route::Direct(_) => path,
    };
    let mut forwarded = format!("{method} {request_target} {version}\r\nHost: {authority}\r\n");
    for header in &headers {
        let proxy_header = header.len() >= 6 && header[..6].eq_ignore_ascii_case("proxy-");
        if !proxy_header && !header_is(header, "connection") && !header_is(header, "host") {
            forwarded.push_str(header);
            forwarded.push_str("\r\n");
        }
    }
    forwarded.push_str("Connection: close\r\n");
    if let Route::Upstream(proxy) = &route {
        forwarded.push_str("Proxy-Connection: close\r\n");
        if let Some(authorization) = &proxy.authorization {
            forwarded.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
        }
    }
    forwarded.push_str("\r\n");
    match connect(&route, &target, false) {
        None => respond(&mut client, "502 Bad Gateway", ""),
        Some((stream, _)) => {
            forward_request(client, stream, forwarded.as_bytes(), &leftover, body_length);
        }
    }
}

fn request_body_length(headers: &[&str]) -> Result<u64, &'static str> {
    if headers
        .iter()
        .any(|header| header_is(header, "transfer-encoding"))
    {
        return Err("411 Length Required");
    }
    let mut length = None;
    for header in headers {
        if !header_is(header, "content-length") {
            continue;
        }
        let value = header
            .split_once(':')
            .and_then(|(_, value)| value.trim().parse::<u64>().ok())
            .ok_or("400 Bad Request")?;
        if length.is_some_and(|seen| seen != value) {
            return Err("400 Bad Request");
        }
        length = Some(value);
    }
    Ok(length.unwrap_or(0))
}

fn forward_request(
    mut client: Client,
    mut server_stream: TcpStream,
    head: &[u8],
    leftover: &[u8],
    body_length: u64,
) {
    if server_stream.write_all(head).is_err() {
        return;
    }
    let from_leftover = leftover
        .len()
        .min(usize::try_from(body_length).unwrap_or(usize::MAX));
    if server_stream.write_all(&leftover[..from_leftover]).is_err() {
        return;
    }
    let remaining = body_length - from_leftover as u64;
    if remaining > 0
        && io::copy(
            &mut Read::by_ref(&mut client).take(remaining),
            &mut server_stream,
        )
        .ok()
            != Some(remaining)
    {
        return;
    }
    let (Ok(mut from_server), Ok(mut to_client)) = (server_stream.try_clone(), client.try_clone())
    else {
        return;
    };
    let downstream = thread::spawn(move || {
        let _ = io::copy(&mut from_server, &mut to_client);
        let _ = to_client.shutdown(Shutdown::Both);
    });
    let mut discarded = [0u8; 4096];
    while matches!(client.read(&mut discarded), Ok(count) if count > 0) {}
    let _ = server_stream.shutdown(Shutdown::Both);
    let _ = downstream.join();
    let _ = client.shutdown(Shutdown::Both);
}

fn handle_socks(mut client: Client, server: &Server) {
    let Some(count) = read_byte(&mut client) else {
        return;
    };
    let mut methods = vec![0u8; usize::from(count)];
    if client.read_exact(&mut methods).is_err() {
        return;
    }
    if !methods.contains(&SOCKS_NO_AUTH) {
        let _ = client.write_all(&[SOCKS_VERSION, SOCKS_NO_ACCEPTABLE_METHOD]);
        return;
    }
    if !write_all(&mut client, &[SOCKS_VERSION, SOCKS_NO_AUTH]) {
        return;
    }
    let mut request = [0u8; 4];
    if client.read_exact(&mut request).is_err() || request[0] != SOCKS_VERSION {
        return;
    }
    let host = match request[3] {
        SOCKS_ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            if client.read_exact(&mut octets).is_err() {
                return;
            }
            Ipv4Addr::from(octets).to_string()
        }
        SOCKS_ATYP_DOMAIN => {
            let Some(length) = read_byte(&mut client) else {
                return;
            };
            let mut name = vec![0u8; usize::from(length)];
            if client.read_exact(&mut name).is_err() {
                return;
            }
            String::from_utf8_lossy(&name).into_owned()
        }
        _ => {
            let _ = client.write_all(&socks_reply(SOCKS_ADDRESS_NOT_SUPPORTED));
            return;
        }
    };
    let mut port = [0u8; 2];
    if client.read_exact(&mut port).is_err() {
        return;
    }
    let _ = client.set_read_timeout(None);
    if request[1] != SOCKS_CONNECT {
        let _ = client.write_all(&socks_reply(SOCKS_COMMAND_NOT_SUPPORTED));
        return;
    }
    let Some(host) = normalize_host(&host) else {
        let _ = client.write_all(&socks_reply(SOCKS_NOT_ALLOWED));
        return;
    };
    let target = Target {
        host,
        port: u16::from_be_bytes(port),
    };
    let route = match admit(server, &target, true) {
        Err(_) => {
            let _ = client.write_all(&socks_reply(SOCKS_NOT_ALLOWED));
            return;
        }
        Ok(Some(route)) => route,
        Ok(None) => {
            let _ = client.write_all(&socks_reply(SOCKS_HOST_UNREACHABLE));
            return;
        }
    };
    match connect(&route, &target, true) {
        None => {
            let _ = client.write_all(&socks_reply(SOCKS_CONNECTION_REFUSED));
        }
        Some((stream, early)) => {
            if write_all(&mut client, &socks_reply(SOCKS_SUCCEEDED))
                && write_all(&mut client, &early)
            {
                relay(client, stream, &[]);
            }
        }
    }
}

fn admit(server: &Server, target: &Target, secure: bool) -> Result<Option<Route>, NetworkReason> {
    let decision = decide(server, target, secure);
    let (allowed, reason) = match &decision {
        Err(reason) => (false, Some(*reason)),
        Ok(_) => (true, None),
    };
    let key = (target.host.clone(), target.port, allowed);
    let first = server
        .seen
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(false);
    if first {
        (server.report)(Network {
            host: target.host.clone(),
            port: target.port,
            allowed,
            reason,
        });
    }
    decision
}

fn decide(server: &Server, target: &Target, secure: bool) -> Result<Option<Route>, NetworkReason> {
    if target.host.is_empty() || target.host.contains(':') {
        return Err(NetworkReason::NotAllowed);
    }
    let allowance = server
        .policy
        .allowance(&target.host, target.port)
        .ok_or(NetworkReason::NotAllowed)?;
    let addresses = resolve(&target.host, target.port);
    let check_private = server.policy.mode != NetworkMode::None && allowance == Allowance::Any;
    if check_private && addresses.iter().any(|address| is_private(*address.ip())) {
        return Err(NetworkReason::PrivateAddress);
    }
    let ips: Vec<Ipv4Addr> = addresses.iter().map(|address| *address.ip()).collect();
    if let Some(proxy) = server.upstream.select(secure, &target.host, &ips) {
        return Ok(Some(Route::Upstream(proxy.clone())));
    }
    if addresses.is_empty() {
        return Ok(None);
    }
    Ok(Some(Route::Direct(addresses)))
}

fn connect(route: &Route, target: &Target, tunnel: bool) -> Option<(TcpStream, Vec<u8>)> {
    match route {
        Route::Direct(addresses) => connect_direct(addresses).map(|stream| (stream, Vec::new())),
        Route::Upstream(proxy) => {
            let stream = connect_direct(&resolve(&proxy.host, proxy.port))?;
            if tunnel {
                tunnel_through(stream, proxy, target)
            } else {
                Some((stream, Vec::new()))
            }
        }
    }
}

fn resolve(host: &str, port: u16) -> Vec<SocketAddrV4> {
    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return vec![SocketAddrV4::new(address, port)];
    }
    (host, port)
        .to_socket_addrs()
        .map(|addresses| {
            addresses
                .filter_map(|address| match address {
                    SocketAddr::V4(address) => Some(address),
                    SocketAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn connect_direct(addresses: &[SocketAddrV4]) -> Option<TcpStream> {
    addresses.iter().find_map(|address| {
        TcpStream::connect_timeout(&SocketAddr::from(*address), CONNECT_TIMEOUT).ok()
    })
}

fn tunnel_through(
    mut stream: TcpStream,
    proxy: &ProxyAddress,
    target: &Target,
) -> Option<(TcpStream, Vec<u8>)> {
    let host_port = target_text(target);
    let mut request = format!("CONNECT {host_port} HTTP/1.1\r\nHost: {host_port}\r\n");
    if let Some(authorization) = &proxy.authorization {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
    let (head, leftover) = read_head(&mut stream, Vec::new())?;
    let _ = stream.set_read_timeout(None);
    let status = String::from_utf8_lossy(&head);
    let code = status
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())?;
    (200..300).contains(&code).then_some((stream, leftover))
}

fn relay(client: Client, server_stream: TcpStream, initial: &[u8]) {
    let Ok(mut to_server) = server_stream.try_clone() else {
        return;
    };
    if !initial.is_empty() && to_server.write_all(initial).is_err() {
        return;
    }
    let Ok(mut from_server) = server_stream.try_clone() else {
        return;
    };
    let Ok(mut to_client) = client.try_clone() else {
        return;
    };
    let downstream = thread::spawn(move || {
        let _ = io::copy(&mut from_server, &mut to_client);
        let _ = to_client.shutdown(Shutdown::Write);
    });
    let mut from_client = client;
    let _ = io::copy(&mut from_client, &mut to_server);
    let _ = to_server.shutdown(Shutdown::Write);
    let _ = downstream.join();
    let _ = from_client.shutdown(Shutdown::Both);
    let _ = server_stream.shutdown(Shutdown::Both);
}

fn read_head(stream: &mut impl Read, mut buffer: Vec<u8>) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_blank_line(&buffer) {
            let leftover = buffer.split_off(end);
            return Some((buffer, leftover));
        }
        if buffer.len() >= HEAD_LIMIT {
            return None;
        }
        let count = stream.read(&mut chunk).ok().filter(|count| *count > 0)?;
        buffer.extend_from_slice(&chunk[..count]);
    }
}

fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn read_byte(stream: &mut impl Read) -> Option<u8> {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).ok().map(|_| byte[0])
}

fn write_all(stream: &mut impl Write, bytes: &[u8]) -> bool {
    stream.write_all(bytes).and_then(|_| stream.flush()).is_ok()
}

fn respond(client: &mut Client, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(response.as_bytes());
    let _ = client.flush();
    let _ = client.shutdown(Shutdown::Both);
}

fn denied_body(target: &Target, reason: NetworkReason) -> String {
    format!(
        "agentrun: {} is not allowed ({})\n",
        target_text(target),
        reason.name()
    )
}

fn socks_reply(code: u8) -> [u8; 10] {
    [SOCKS_VERSION, code, 0, SOCKS_ATYP_IPV4, 0, 0, 0, 0, 0, 0]
}

fn header_is(header: &str, name: &str) -> bool {
    header
        .split_once(':')
        .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case(name))
}

fn split_absolute_url(target: &str) -> Option<(String, String)> {
    let prefix = target
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("http://"))?;
    let rest = &target[prefix.len()..];
    let (authority, path) = match rest.find(['/', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    let authority = authority
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(authority);
    Some((authority.to_string(), path))
}

fn parse_host_port(authority: &str, default_port: Option<u16>) -> Option<Target> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        let port = match rest.strip_prefix(':') {
            Some(port) => Some(port.parse::<u16>().ok()?),
            None => None,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, _)) if host.contains(':') => (authority, None),
            Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
            None => (authority, None),
        }
    };
    let port = port.or(default_port)?;
    Some(Target {
        host: normalize_host(host)?,
        port,
    })
}

fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_lowercase();
    if host.is_empty() || host.len() > HOST_NAME_LIMIT {
        return None;
    }
    let ipv6_literal = host.contains(':')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.');
    (ipv6_literal || valid_host_name(&host)).then_some(host)
}

fn valid_host_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= HOST_NAME_LIMIT
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn target_text(target: &Target) -> String {
    format!("{}:{}", target.host, target.port)
}

fn authority_text(target: &Target) -> String {
    if target.port == HTTP_DEFAULT_PORT {
        target.host.clone()
    } else {
        target_text(target)
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            decoded.push(value);
            index += 3;
            continue;
        }
        decoded.push(byte);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let number = u32::from(group[0]) << 16 | u32::from(group[1]) << 8 | u32::from(group[2]);
        encoded.push(TABLE[(number >> 18) as usize & 63] as char);
        encoded.push(TABLE[(number >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[(number >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[number as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|item| item.to_string()).collect()
    }

    fn rule(host: &str, wildcard: bool, port: Option<u16>) -> HostRule {
        HostRule {
            host: host.to_string(),
            wildcard,
            port,
        }
    }

    #[test]
    fn proxy_runs_only_with_a_sandbox_and_depends_on_the_runtime() {
        for mode in [NetworkMode::None, NetworkMode::Full, NetworkMode::Custom] {
            assert!(proxy_needed(Runtime::Pi, mode, true), "{mode:?}");
            assert!(!proxy_needed(Runtime::Pi, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::ClaudeCode, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::Codex, mode, true), "{mode:?}");
        }
        assert!(!proxy_needed(Runtime::ClaudeCode, NetworkMode::None, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Full, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Custom, true));
    }

    #[test]
    fn proxy_environment_sets_both_cases_and_clears_no_proxy() {
        let env = proxy_environment("4321");
        let names: Vec<String> = env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "HTTPS_PROXY",
                "HTTP_PROXY",
                "ALL_PROXY",
                "https_proxy",
                "http_proxy",
                "all_proxy",
                "NO_PROXY",
                "no_proxy"
            ]
        );
        for (name, value) in &env {
            if name.to_string_lossy().to_lowercase() == "no_proxy" {
                assert!(value.is_empty());
            } else {
                assert_eq!(value, "http://127.0.0.1:4321");
            }
        }
    }

    #[test]
    fn usage_checks_in_order() {
        assert_eq!(
            check_usage(NetworkMode::Custom, &[]),
            Err("--network custom requires at least one --allow-host".to_string())
        );
        for mode in [NetworkMode::None, NetworkMode::Full] {
            assert_eq!(
                check_usage(mode, &strings(&["example.com"])),
                Err("--allow-host requires --network custom".to_string())
            );
        }
        assert_eq!(
            check_usage(NetworkMode::Custom, &strings(&["example.com", "*"])),
            Err("--allow-host '*' is not allowed. Use --network full".to_string())
        );
        assert_eq!(check_usage(NetworkMode::None, &[]), Ok(Vec::new()));
    }

    #[test]
    fn allow_host_values_parse_and_normalise() {
        let cases = [
            ("example.com", rule("example.com", false, None)),
            ("Example.COM.", rule("example.com", false, None)),
            ("example.com:8080", rule("example.com", false, Some(8080))),
            ("*.example.com", rule("example.com", true, None)),
            ("*.Example.com:443", rule("example.com", true, Some(443))),
            ("127.0.0.1:9000", rule("127.0.0.1", false, Some(9000))),
            ("10.0.0.1", rule("10.0.0.1", false, None)),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_allow_host(value), Ok(expected), "{value}");
        }
        assert_eq!(
            check_usage(
                NetworkMode::Custom,
                &strings(&["a.example", "*.b.example:1"])
            ),
            Ok(vec![
                rule("a.example", false, None),
                rule("b.example", true, Some(1))
            ])
        );
    }

    #[test]
    fn allow_host_usage_errors() {
        let wildcard = |value: &str| {
            format!(
                "--allow-host '{value}': a wildcard is only allowed as the first label, as in *.example.com"
            )
        };
        let malformed = |value: &str| format!("--allow-host '{value}': expected HOST or HOST:PORT");
        let ipv6 =
            |value: &str| format!("--allow-host '{value}': IPv6 addresses are not supported");
        let cases: Vec<(&str, String)> = vec![
            (
                "*",
                "--allow-host '*' is not allowed. Use --network full".to_string(),
            ),
            ("*:443", wildcard("*:443")),
            ("api.*.example.com", wildcard("api.*.example.com")),
            ("*.*.example.com", wildcard("*.*.example.com")),
            ("*example.com", wildcard("*example.com")),
            ("example.*", wildcard("example.*")),
            ("", malformed("")),
            (".", malformed(".")),
            ("*.", wildcard("*.")),
            (":443", malformed(":443")),
            ("example.com:", malformed("example.com:")),
            ("example.com:0", malformed("example.com:0")),
            ("example.com:65536", malformed("example.com:65536")),
            ("example.com:abc", malformed("example.com:abc")),
            ("exa mple.com", malformed("exa mple.com")),
            ("exa#mple.com", malformed("exa#mple.com")),
            ("*.exa/mple.com", malformed("*.exa/mple.com")),
            ("[::1]:443", ipv6("[::1]:443")),
            ("[::1]", ipv6("[::1]")),
            ("::1", ipv6("::1")),
            ("2001:db8::1", ipv6("2001:db8::1")),
            ("fe80::1:443", ipv6("fe80::1:443")),
        ];
        for (value, expected) in cases {
            assert_eq!(parse_allow_host(value), Err(expected), "{value}");
        }
    }

    #[test]
    fn rule_matching_exact_wildcard_and_ports() {
        let exact = rule("example.com", false, None);
        assert!(exact.matches("example.com", 443));
        assert!(exact.matches("example.com", 80));
        assert!(!exact.matches("example.com", 8080));
        assert!(!exact.matches("api.example.com", 443));
        assert!(!exact.matches("notexample.com", 443));
        let with_port = rule("example.com", false, Some(8080));
        assert!(with_port.matches("example.com", 8080));
        assert!(!with_port.matches("example.com", 443));
        let wildcard = rule("example.com", true, None);
        assert!(wildcard.matches("api.example.com", 443));
        assert!(wildcard.matches("a.b.example.com", 80));
        assert!(!wildcard.matches("example.com", 443));
        assert!(!wildcard.matches("notexample.com", 443));
        assert!(!wildcard.matches("api.example.com", 8443));
        assert_eq!(wildcard.pattern(), "*.example.com");
        assert_eq!(exact.pattern(), "example.com");
    }

    #[test]
    fn policy_allowances_per_mode() {
        let policy = |mode: NetworkMode| Policy {
            mode,
            rules: vec![rule("pypi.org", false, None)],
            service_hosts: strings(&["api.deepseek.com"]),
        };
        let full = policy(NetworkMode::Full);
        assert_eq!(
            full.allowance("anything.example", 1234),
            Some(Allowance::Any)
        );
        let custom = policy(NetworkMode::Custom);
        assert_eq!(custom.allowance("pypi.org", 443), Some(Allowance::Listed));
        let wildcard = Policy {
            mode: NetworkMode::Custom,
            rules: vec![
                rule("example.com", true, None),
                rule("exact.example.com", false, None),
            ],
            service_hosts: Vec::new(),
        };
        assert_eq!(
            wildcard.allowance("files.example.com", 443),
            Some(Allowance::Any)
        );
        assert_eq!(
            wildcard.allowance("exact.example.com", 443),
            Some(Allowance::Listed)
        );
        assert_eq!(wildcard.allowance("example.com", 443), None);
        assert_eq!(
            custom.allowance("api.deepseek.com", 443),
            Some(Allowance::Service)
        );
        assert_eq!(custom.allowance("api.deepseek.com", 80), None);
        assert_eq!(custom.allowance("example.com", 443), None);
        let none = policy(NetworkMode::None);
        assert_eq!(none.allowance("pypi.org", 443), None);
        assert_eq!(
            none.allowance("api.deepseek.com", 443),
            Some(Allowance::Service)
        );
    }

    #[test]
    fn private_ranges_each_segment() {
        let private = [
            "0.0.0.0",
            "0.255.255.255",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
        ];
        for address in private {
            assert!(is_private(address.parse().unwrap()), "{address}");
        }
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.0",
            "128.0.0.1",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.18.0.1",
            "203.0.113.5",
        ];
        for address in public {
            assert!(!is_private(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn no_proxy_items() {
        let rules = parse_no_proxy(
            "localhost, 127.0.0.1,.internal.example,*.corp.example,10.0.0.0/8,Example.COM:8080,,bad/cidr",
        );
        assert_eq!(
            rules,
            vec![
                NoProxyRule::Host("localhost".to_string()),
                NoProxyRule::Host("127.0.0.1".to_string()),
                NoProxyRule::Suffix("internal.example".to_string()),
                NoProxyRule::Suffix("corp.example".to_string()),
                NoProxyRule::Cidr(Ipv4Addr::new(10, 0, 0, 0), 8),
                NoProxyRule::Host("example.com".to_string()),
            ]
        );
        let matches = |host: &str, addresses: &[Ipv4Addr]| {
            rules.iter().any(|rule| rule.matches(host, addresses))
        };
        assert!(matches("localhost", &[]));
        assert!(matches("127.0.0.1", &[]));
        assert!(matches("internal.example", &[]));
        assert!(matches("db.internal.example", &[]));
        assert!(!matches("notinternal.example", &[]));
        assert!(matches("a.corp.example", &[]));
        assert!(matches("10.1.2.3", &[]));
        assert!(matches("resolved.example", &[Ipv4Addr::new(10, 9, 9, 9)]));
        assert!(!matches("resolved.example", &[Ipv4Addr::new(11, 9, 9, 9)]));
        assert!(matches("example.com", &[]));
        assert!(!matches("example.org", &[]));
        assert_eq!(parse_no_proxy("*"), vec![NoProxyRule::All]);
        assert!(NoProxyRule::All.matches("anything", &[]));
    }

    #[test]
    fn upstream_address_and_authorization() {
        assert_eq!(
            ProxyAddress::parse("http://proxy.example:3128"),
            Some(ProxyAddress {
                host: "proxy.example".to_string(),
                port: 3128,
                authorization: None,
            })
        );
        assert_eq!(
            ProxyAddress::parse("HTTP://proxy.example/"),
            Some(ProxyAddress {
                host: "proxy.example".to_string(),
                port: 80,
                authorization: None,
            })
        );
        assert_eq!(
            ProxyAddress::parse("127.0.0.1:46201"),
            Some(ProxyAddress {
                host: "127.0.0.1".to_string(),
                port: 46201,
                authorization: None,
            })
        );
        assert_eq!(
            ProxyAddress::parse("http://user:p%40ss@proxy.example:8080/path"),
            Some(ProxyAddress {
                host: "proxy.example".to_string(),
                port: 8080,
                authorization: Some("Basic dXNlcjpwQHNz".to_string()),
            })
        );
        assert_eq!(
            ProxyAddress::parse("http://user@proxy.example")
                .unwrap()
                .authorization,
            Some("Basic dXNlcjo=".to_string())
        );
        for value in [
            "socks5://127.0.0.1:1080",
            "https://proxy.example",
            "http://",
            "http://:80",
        ] {
            assert_eq!(ProxyAddress::parse(value), None, "{value}");
        }
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(percent_decode("a%20b%zz%4"), "a b%zz%4");
    }

    #[test]
    fn upstream_from_env_prefers_lowercase_and_reports_unsupported_schemes() {
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect::<BTreeMap<_, _>>()
        };
        let (upstream, notes) = Upstream::from_env(&env(&[
            ("https_proxy", "http://secure.example:1"),
            ("HTTPS_PROXY", "http://ignored.example:2"),
            ("ALL_PROXY", "http://all.example:3"),
            ("no_proxy", "localhost"),
            ("NO_PROXY", ".corp.example"),
        ]));
        assert!(notes.is_empty());
        assert_eq!(upstream.secure.as_ref().unwrap().host, "secure.example");
        assert_eq!(upstream.plain.as_ref().unwrap().host, "all.example");
        assert_eq!(
            upstream.no_proxy,
            vec![NoProxyRule::Host("localhost".to_string())]
        );
        assert!(upstream.select(true, "example.com", &[]).is_some());
        assert!(upstream.select(true, "localhost", &[]).is_none());
        assert!(upstream.select(false, "example.com", &[]).is_some());

        let (upstream, notes) = Upstream::from_env(&env(&[
            ("https_proxy", ""),
            ("HTTPS_PROXY", "socks5://127.0.0.1:1080"),
            ("http_proxy", "http://plain.example"),
        ]));
        assert_eq!(
            notes,
            vec!["upstream proxy ignored: HTTPS_PROXY uses an unsupported scheme"]
        );
        assert_eq!(upstream.secure, None);
        assert_eq!(upstream.plain.as_ref().unwrap().host, "plain.example");
        assert_eq!(upstream.no_proxy, Vec::new());

        let (upstream, notes) = Upstream::from_env(&env(&[]));
        assert_eq!(upstream, Upstream::default());
        assert!(notes.is_empty());
    }

    #[test]
    fn request_targets_and_absolute_urls() {
        let target = parse_host_port("Example.COM.:8443", None).unwrap();
        assert_eq!((target.host.as_str(), target.port), ("example.com", 8443));
        let target = parse_host_port("example.com", Some(80)).unwrap();
        assert_eq!((target.host.as_str(), target.port), ("example.com", 80));
        assert!(parse_host_port("example.com", None).is_none());
        assert!(parse_host_port("example.com:x", None).is_none());
        let target = parse_host_port("[::1]:443", None).unwrap();
        assert_eq!((target.host.as_str(), target.port), ("::1", 443));
        let target = parse_host_port("::1", Some(80)).unwrap();
        assert_eq!((target.host.as_str(), target.port), ("::1", 80));
        for invalid in ["bad host:80", "a\u{1}b:443", "exa#mple.com:80", "a:b:g", ""] {
            assert!(parse_host_port(invalid, Some(80)).is_none(), "{invalid}");
        }
        assert_eq!(
            normalize_host("Sub_Host-1.Example.COM."),
            Some("sub_host-1.example.com".to_string())
        );
        assert_eq!(
            normalize_host("2001:DB8::1"),
            Some("2001:db8::1".to_string())
        );
        assert_eq!(normalize_host(&"a".repeat(254)), None);
        assert_eq!(normalize_host("x\ny"), None);
        assert_eq!(
            split_absolute_url("http://user@Example.com:8080/a/b?c=d"),
            Some(("Example.com:8080".to_string(), "/a/b?c=d".to_string()))
        );
        assert_eq!(
            split_absolute_url("HTTP://example.com"),
            Some(("example.com".to_string(), "/".to_string()))
        );
        assert_eq!(
            split_absolute_url("http://example.com?x=1"),
            Some(("example.com".to_string(), "/?x=1".to_string()))
        );
        assert_eq!(split_absolute_url("https://example.com/"), None);
        assert_eq!(split_absolute_url("/relative"), None);
        assert!(header_is("Connection: keep-alive", "connection"));
        assert!(header_is("connection : close", "Connection"));
        assert!(!header_is("Proxy-Connection: close", "connection"));
        assert_eq!(
            find_blank_line(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody"),
            Some(27)
        );
        assert_eq!(find_blank_line(b"partial\r\n"), None);
    }
}
