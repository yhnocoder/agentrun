use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io::{self, ErrorKind, Read, Write};
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

use super::address::{Target, is_private, target_text};
use super::http::handle_http;
use super::rule::{Allowance, Policy};
use super::socks::{SOCKS_VERSION, handle_socks};
use super::upstream::{ProxyAddress, Upstream};
use super::{LISTEN_ADDRESS, ProxyEndpoint, SOCKET_FILE};
use crate::cli::NetworkMode;
use crate::output::{Network, NetworkReason};

const HEAD_LIMIT: usize = 64 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_UPSTREAM_RESPONSE: &str = "upstream proxy answered an invalid response";
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

type Report = Box<dyn Fn(Network) + Send + Sync>;

pub(super) struct Server {
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
    pub fn bind(tempdir: &Path) -> Result<FilterProxy, String> {
        Self::listen(tempdir).map_err(|error| format!("cannot start the filter proxy: {error}"))
    }

    fn listen(tempdir: &Path) -> io::Result<FilterProxy> {
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
    pub fn serve_session(
        &mut self,
        policy: Policy,
        env: &BTreeMap<OsString, OsString>,
        report: impl Fn(Network) + Send + Sync + 'static,
    ) -> Vec<String> {
        let (upstream, notes) = Upstream::from_env(env);
        self.serve(policy, upstream, report);
        notes
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
        match accept() {
            Some(client) => {
                let _ = client.set_nonblocking(false);
                let server = Arc::clone(server);
                thread::spawn(move || handle(client, &server));
            }
            None => thread::sleep(STOP_POLL_INTERVAL),
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

pub(super) enum Client {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Client {
    pub(super) fn try_clone(&self) -> io::Result<Client> {
        Ok(match self {
            Client::Tcp(stream) => Client::Tcp(stream.try_clone()?),
            Client::Unix(stream) => Client::Unix(stream.try_clone()?),
        })
    }
    pub(super) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
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
    pub(super) fn shutdown(&self, how: Shutdown) -> io::Result<()> {
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

pub(super) enum Route {
    Direct(Vec<SocketAddrV4>),
    Upstream(ProxyAddress),
}

pub(super) enum Decision {
    Denied(NetworkReason),
    Unreachable,
    Route(Route),
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

pub(super) fn admit(server: &Server, target: &Target, secure: bool) -> Decision {
    let decision = decide(server, target, secure);
    let allowed = !matches!(decision, Decision::Denied(_));
    let reason = match &decision {
        Decision::Denied(reason) => Some(*reason),
        Decision::Unreachable | Decision::Route(_) => None,
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

fn decide(server: &Server, target: &Target, secure: bool) -> Decision {
    if target.host.is_empty() || target.host.contains(':') {
        return Decision::Denied(NetworkReason::NotAllowed);
    }
    let Some(allowance) = server.policy.allowance(&target.host, target.port) else {
        return Decision::Denied(NetworkReason::NotAllowed);
    };
    let addresses = resolve(&target.host, target.port);
    let check_private = server.policy.mode != NetworkMode::None && allowance == Allowance::Any;
    if check_private && addresses.iter().any(|address| is_private(*address.ip())) {
        return Decision::Denied(NetworkReason::PrivateAddress);
    }
    let ips: Vec<Ipv4Addr> = addresses.iter().map(|address| *address.ip()).collect();
    if let Some(proxy) = server.upstream.select(secure, &target.host, &ips) {
        return Decision::Route(Route::Upstream(proxy.clone()));
    }
    if addresses.is_empty() {
        return Decision::Unreachable;
    }
    Decision::Route(Route::Direct(addresses))
}

pub(super) fn connect(
    route: &Route,
    target: &Target,
    tunnel: bool,
) -> Result<(TcpStream, Vec<u8>), String> {
    match route {
        Route::Direct(addresses) => connect_direct(addresses)
            .map(|stream| (stream, Vec::new()))
            .map_err(|error| format!("connection failed ({error})")),
        Route::Upstream(proxy) => {
            let stream =
                connect_direct(&resolve(&proxy.host, proxy.port)).map_err(upstream_failure)?;
            if tunnel {
                tunnel_through(stream, proxy, target)
            } else {
                Ok((stream, Vec::new()))
            }
        }
    }
}

fn upstream_failure(error: impl std::fmt::Display) -> String {
    format!("upstream proxy connection failed ({error})")
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

fn connect_direct(addresses: &[SocketAddrV4]) -> Result<TcpStream, String> {
    let mut failure = "no IPv4 address".to_string();
    for address in addresses {
        match TcpStream::connect_timeout(&SocketAddr::from(*address), CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => failure = error.to_string(),
        }
    }
    Err(failure)
}

fn tunnel_through(
    mut stream: TcpStream,
    proxy: &ProxyAddress,
    target: &Target,
) -> Result<(TcpStream, Vec<u8>), String> {
    let host_port = target_text(target);
    let mut request = format!("CONNECT {host_port} HTTP/1.1\r\nHost: {host_port}\r\n");
    if let Some(authorization) = &proxy.authorization {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(upstream_failure)?;
    let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
    let (head, leftover) = read_head(&mut stream, Vec::new()).map_err(|error| match error {
        HeadError::TooLarge => INVALID_UPSTREAM_RESPONSE.to_string(),
        HeadError::Closed => "upstream proxy closed the connection".to_string(),
        HeadError::Read(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            "upstream proxy did not answer in time".to_string()
        }
        HeadError::Read(error) => upstream_failure(error),
    })?;
    let _ = stream.set_read_timeout(None);
    let status = String::from_utf8_lossy(&head);
    let code = status
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(INVALID_UPSTREAM_RESPONSE)?;
    if (200..300).contains(&code) {
        Ok((stream, leftover))
    } else {
        Err(format!("upstream proxy answered {code}"))
    }
}

pub(super) fn relay(
    client: Client,
    server_stream: TcpStream,
    initial: &[u8],
    idle_timeout: Duration,
) {
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
    let _ = from_server.set_read_timeout(Some(idle_timeout));
    let client_closed = Arc::new(AtomicBool::new(false));
    let downstream = {
        let client_closed = Arc::clone(&client_closed);
        thread::spawn(move || {
            copy_until_idle(&mut from_server, &mut to_client, &client_closed);
            let _ = to_client.shutdown(Shutdown::Write);
        })
    };
    let mut from_client = client;
    let _ = io::copy(&mut from_client, &mut to_server);
    let _ = to_server.shutdown(Shutdown::Write);
    client_closed.store(true, Ordering::SeqCst);
    let _ = downstream.join();
    let _ = from_client.shutdown(Shutdown::Both);
    let _ = server_stream.shutdown(Shutdown::Both);
}

pub(super) fn copy_until_idle(
    from_server: &mut TcpStream,
    to_client: &mut Client,
    client_closed: &AtomicBool,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match from_server.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => {
                if to_client.write_all(&buffer[..count]).is_err() {
                    return;
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if client_closed.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

pub(super) enum HeadError {
    TooLarge,
    Closed,
    Read(io::Error),
}

pub(super) fn read_head(
    stream: &mut impl Read,
    mut buffer: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), HeadError> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find_blank_line(&buffer) {
            let leftover = buffer.split_off(end);
            return Ok((buffer, leftover));
        }
        if buffer.len() >= HEAD_LIMIT {
            return Err(HeadError::TooLarge);
        }
        let count = match stream.read(&mut chunk) {
            Ok(0) => return Err(HeadError::Closed),
            Ok(count) => count,
            Err(error) => return Err(HeadError::Read(error)),
        };
        buffer.extend_from_slice(&chunk[..count]);
    }
}

fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

pub(super) fn write_all(stream: &mut impl Write, bytes: &[u8]) -> bool {
    stream.write_all(bytes).and_then(|_| stream.flush()).is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn open_tunnel(idle: Duration) -> (TcpStream, TcpStream, mpsc::Receiver<()>) {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let clients = TcpListener::bind("127.0.0.1:0").unwrap();
        let caller = TcpStream::connect(clients.local_addr().unwrap()).unwrap();
        let (proxy_side, _) = clients.accept().unwrap();
        let server_stream = TcpStream::connect(upstream.local_addr().unwrap()).unwrap();
        let (upstream_side, _) = upstream.accept().unwrap();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            relay(Client::Tcp(proxy_side), server_stream, b"early ", idle);
            let _ = done.send(());
        });
        for stream in [&caller, &upstream_side] {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
        }
        (caller, upstream_side, finished)
    }

    #[test]
    fn tunnel_with_a_silent_upstream_ends_after_the_client_closes() {
        let (mut caller, mut upstream_side, finished) = open_tunnel(Duration::from_millis(200));
        caller.write_all(b"hello").unwrap();
        caller.shutdown(Shutdown::Write).unwrap();
        let mut received = Vec::new();
        upstream_side.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"early hello");
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the tunnel did not end");
        let mut rest = Vec::new();
        assert_eq!(caller.read_to_end(&mut rest).unwrap(), 0);
        drop(upstream_side);
    }

    #[test]
    fn tunnel_relays_upstream_gaps_longer_than_the_idle_timeout_while_the_client_is_open() {
        let (mut caller, mut upstream_side, finished) = open_tunnel(Duration::from_millis(100));
        let sender = thread::spawn(move || {
            for part in [&b"first "[..], b"second ", b"third"] {
                thread::sleep(Duration::from_millis(350));
                upstream_side.write_all(part).unwrap();
            }
            upstream_side.shutdown(Shutdown::Write).unwrap();
            upstream_side
        });
        let mut received = Vec::new();
        caller.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"first second third");
        caller.shutdown(Shutdown::Write).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the tunnel did not end");
        drop(sender.join().unwrap());
    }

    #[test]
    fn read_head_reports_too_large_closed_and_read_errors() {
        let mut endless = io::repeat(b'a');
        assert!(matches!(
            read_head(&mut endless, Vec::new()),
            Err(HeadError::TooLarge)
        ));
        let mut cut = &b"HTTP/1.1 200 OK\r\n"[..];
        assert!(matches!(
            read_head(&mut cut, Vec::new()),
            Err(HeadError::Closed)
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut silent = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let timed_out = read_head(&mut silent, Vec::new());
        assert!(
            matches!(
                &timed_out,
                Err(HeadError::Read(error))
                    if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
            ),
            "the read did not time out"
        );
        let mut complete = &b"HTTP/1.1 200 OK\r\n\r\nrest"[..];
        let Ok((head, leftover)) = read_head(&mut complete, Vec::new()) else {
            panic!("the complete head was not read");
        };
        assert_eq!(head, b"HTTP/1.1 200 OK\r\n\r\n");
        assert_eq!(leftover, b"rest");
    }

    #[test]
    fn head_ends_at_the_first_blank_line() {
        assert_eq!(
            find_blank_line(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody"),
            Some(27)
        );
        assert_eq!(find_blank_line(b"partial\r\n"), None);
    }
}
