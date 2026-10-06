use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agentrun::cli::NetworkMode;
use agentrun::network::{FilterProxy, HostRule, Policy, ProxyAddress, Upstream, parse_no_proxy};
use agentrun::output::{Network, NetworkReason};
use tempfile::TempDir;

const BASIC_USER_PASS: &str = "Basic dXNlcjpwYXNz";

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(proxy_like: bool) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let seen = Arc::clone(&seen);
                thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut head = read_head(&mut stream);
                    if let Some(length) = content_length(&head) {
                        let mut body = vec![0u8; length];
                        if stream.read_exact(&mut body).is_ok() {
                            head.push_str(&String::from_utf8_lossy(&body));
                        }
                    }
                    seen.lock().unwrap().push(head.clone());
                    if proxy_like && head.starts_with("CONNECT ") {
                        stream
                            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                            .unwrap();
                        let inner = read_head(&mut stream);
                        seen.lock().unwrap().push(inner);
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                    let _ = stream.shutdown(Shutdown::Both);
                });
            }
        });
        Server { port, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn address(&self) -> ProxyAddress {
        ProxyAddress {
            host: "127.0.0.1".to_string(),
            port: self.port,
            authorization: Some(BASIC_USER_PASS.to_string()),
        }
    }
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

fn read_head(stream: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => bytes.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn read_all(stream: &mut impl Read) -> String {
    let mut text = String::new();
    let _ = stream.read_to_string(&mut text);
    text
}

fn body_of(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

fn start_upstream_answering(answer: &'static [u8]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            read_head(&mut stream);
            let _ = stream.write_all(answer);
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    port
}

fn upstream_at(address: ProxyAddress) -> Upstream {
    Upstream {
        secure: Some(address.clone()),
        plain: Some(address),
        no_proxy: Vec::new(),
    }
}

fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Running {
    proxy: FilterProxy,
    events: Receiver<Network>,
    _tempdir: TempDir,
}

impl Running {
    fn start(policy: Policy, upstream: Upstream) -> Running {
        let tempdir = tempfile::tempdir().unwrap();
        let mut proxy = FilterProxy::bind(tempdir.path()).unwrap();
        let (sender, events) = channel();
        proxy.serve(policy, upstream, move |network| {
            let _ = sender.send(network);
        });
        Running {
            proxy,
            events,
            _tempdir: tempdir,
        }
    }

    fn tcp(&self) -> TcpStream {
        let stream =
            TcpStream::connect(("127.0.0.1", self.proxy.endpoint().port.unwrap())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    }

    fn http(&self, request: &str) -> String {
        let mut stream = self.tcp();
        stream.write_all(request.as_bytes()).unwrap();
        read_all(&mut stream)
    }

    fn events(&self) -> Vec<Network> {
        self.events.try_iter().collect()
    }
}

fn event(host: &str, port: u16, reason: Option<NetworkReason>) -> Network {
    Network {
        host: host.to_string(),
        port,
        allowed: reason.is_none(),
        reason,
    }
}

fn rule(host: &str, port: Option<u16>) -> HostRule {
    HostRule {
        host: host.to_string(),
        wildcard: false,
        port,
    }
}

fn custom(rules: Vec<HostRule>) -> Policy {
    Policy {
        mode: NetworkMode::Custom,
        rules,
        service_hosts: Vec::new(),
    }
}

fn full() -> Policy {
    Policy {
        mode: NetworkMode::Full,
        rules: Vec::new(),
        service_hosts: Vec::new(),
    }
}

fn connect_request(host_port: &str) -> String {
    format!("CONNECT {host_port} HTTP/1.1\r\nHost: {host_port}\r\n\r\n")
}

fn get_through_tunnel(stream: &mut TcpStream) -> String {
    stream
        .write_all(b"GET /tunnel HTTP/1.1\r\nHost: inner\r\nConnection: close\r\n\r\n")
        .unwrap();
    read_all(stream)
}

#[test]
fn connect_is_tunnelled_when_the_host_is_listed() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(server.port))]),
        Upstream::default(),
    );
    let mut stream = running.tcp();
    stream
        .write_all(connect_request(&format!("127.0.0.1:{}", server.port)).as_bytes())
        .unwrap();
    assert_eq!(
        read_head(&mut stream),
        "HTTP/1.1 200 Connection Established\r\n\r\n"
    );
    let response = get_through_tunnel(&mut stream);
    assert!(response.ends_with("\r\n\r\nok"), "{response}");
    assert_eq!(server.requests().len(), 1);
    assert!(server.requests()[0].starts_with("GET /tunnel HTTP/1.1\r\n"));
    assert_eq!(
        running.events(),
        vec![event("127.0.0.1", server.port, None)]
    );
}

#[test]
fn connect_to_an_unlisted_host_gets_403_with_a_body() {
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(1))]),
        Upstream::default(),
    );
    let response = running.http(&connect_request("other.example:443"));
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{response}"
    );
    assert!(
        response.ends_with("\r\n\r\nagentrun: other.example:443 is not allowed (not_allowed)\n"),
        "{response}"
    );
    let again = running.http(&connect_request("Other.Example.:443"));
    assert!(again.starts_with("HTTP/1.1 403"), "{again}");
    assert_eq!(
        running.events(),
        vec![event("other.example", 443, Some(NetworkReason::NotAllowed))]
    );
}

#[test]
fn connect_to_a_closed_port_gets_502() {
    let port = closed_port();
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(port))]),
        Upstream::default(),
    );
    let response = running.http(&connect_request(&format!("127.0.0.1:{port}")));
    assert!(
        response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "{response}"
    );
    assert!(
        body_of(&response).starts_with(&format!("agentrun: 127.0.0.1:{port} connection failed (")),
        "{response}"
    );
    assert_eq!(running.events(), vec![event("127.0.0.1", port, None)]);
}

#[test]
fn connect_to_an_ipv6_literal_is_denied() {
    let running = Running::start(full(), Upstream::default());
    let response = running.http(&connect_request("[::1]:443"));
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{response}"
    );
    assert!(
        response.contains("agentrun: ::1:443 is not allowed (not_allowed)"),
        "{response}"
    );
    assert_eq!(
        running.events(),
        vec![event("::1", 443, Some(NetworkReason::NotAllowed))]
    );
}

#[test]
fn full_denies_private_addresses_unless_listed() {
    let server = Server::start(false);
    let running = Running::start(full(), Upstream::default());
    let response = running.http(&connect_request(&format!("127.0.0.1:{}", server.port)));
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("(private_address)"), "{response}");
    let response = running.http(&connect_request(&format!("localhost:{}", server.port)));
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(response.contains("(private_address)"), "{response}");
    assert_eq!(
        running.events(),
        vec![
            event(
                "127.0.0.1",
                server.port,
                Some(NetworkReason::PrivateAddress)
            ),
            event(
                "localhost",
                server.port,
                Some(NetworkReason::PrivateAddress)
            ),
        ]
    );
    let listed = Running::start(
        custom(vec![rule("localhost", Some(server.port))]),
        Upstream::default(),
    );
    let mut stream = listed.tcp();
    stream
        .write_all(connect_request(&format!("localhost:{}", server.port)).as_bytes())
        .unwrap();
    assert!(read_head(&mut stream).starts_with("HTTP/1.1 200"));
    assert!(get_through_tunnel(&mut stream).ends_with("ok"));
    assert_eq!(listed.events(), vec![event("localhost", server.port, None)]);
}

#[test]
fn service_hosts_skip_the_private_check_and_other_hosts_stay_denied_in_none_mode() {
    let server = Server::start(false);
    let policy = Policy {
        mode: NetworkMode::None,
        rules: Vec::new(),
        service_hosts: vec!["localhost".to_string()],
    };
    let running = Running::start(policy, Upstream::default());
    let response = running.http(&connect_request(&format!("localhost:{}", server.port)));
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    let response = running.http(&connect_request("localhost:443"));
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert!(
        body_of(&response).starts_with("agentrun: localhost:443 connection failed ("),
        "{response}"
    );
    assert_eq!(
        running.events(),
        vec![
            event("localhost", server.port, Some(NetworkReason::NotAllowed)),
            event("localhost", 443, None),
        ]
    );
}

#[test]
fn plain_http_is_forwarded_directly_without_proxy_headers() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(server.port))]),
        Upstream::default(),
    );
    let response = running.http(&format!(
        "GET http://127.0.0.1:{}/path?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nX-Test: yes\r\n\r\n",
        server.port, server.port
    ));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("ok"), "{response}");
    assert_eq!(
        server.requests(),
        vec![format!(
            "GET /path?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Test: yes\r\nConnection: close\r\n\r\n",
            server.port
        )]
    );
    assert_eq!(
        running.events(),
        vec![event("127.0.0.1", server.port, None)]
    );
}

#[test]
fn plain_http_goes_to_the_upstream_with_authorization() {
    let upstream = Server::start(true);
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        Upstream {
            secure: None,
            plain: Some(upstream.address()),
            no_proxy: Vec::new(),
        },
    );
    let response = running.http(
        "GET http://example.com/index.html HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic old\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(
        upstream.requests(),
        vec![format!(
            "GET http://example.com/index.html HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\nProxy-Connection: close\r\nProxy-Authorization: {BASIC_USER_PASS}\r\n\r\n"
        )]
    );
    assert_eq!(running.events(), vec![event("example.com", 80, None)]);
}

#[test]
fn connect_goes_through_the_upstream_tunnel() {
    let upstream = Server::start(true);
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        Upstream {
            secure: Some(upstream.address()),
            plain: None,
            no_proxy: Vec::new(),
        },
    );
    let mut stream = running.tcp();
    stream
        .write_all(connect_request("example.com:443").as_bytes())
        .unwrap();
    assert_eq!(
        read_head(&mut stream),
        "HTTP/1.1 200 Connection Established\r\n\r\n"
    );
    let response = get_through_tunnel(&mut stream);
    assert!(response.ends_with("ok"), "{response}");
    assert_eq!(
        upstream.requests(),
        vec![
            format!(
                "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: {BASIC_USER_PASS}\r\n\r\n"
            ),
            "GET /tunnel HTTP/1.1\r\nHost: inner\r\nConnection: close\r\n\r\n".to_string(),
        ]
    );
}

#[test]
fn unresolvable_hosts_go_to_the_upstream_or_fail_with_502() {
    let upstream = Server::start(true);
    let running = Running::start(
        custom(vec![rule("nonexistent.invalid", None)]),
        Upstream {
            secure: Some(upstream.address()),
            plain: None,
            no_proxy: Vec::new(),
        },
    );
    let mut stream = running.tcp();
    stream
        .write_all(connect_request("nonexistent.invalid:443").as_bytes())
        .unwrap();
    assert!(read_head(&mut stream).starts_with("HTTP/1.1 200"));
    assert!(upstream.requests()[0].starts_with("CONNECT nonexistent.invalid:443 HTTP/1.1\r\n"));
    if let Ok(addresses) = std::net::ToSocketAddrs::to_socket_addrs("nonexistent.invalid:443") {
        eprintln!(
            "skipped the direct 502 check: the local DNS rewrites resolution results (for example fake-ip) and resolved nonexistent.invalid to {:?}",
            addresses.collect::<Vec<_>>()
        );
        return;
    }
    let direct = Running::start(
        custom(vec![rule("nonexistent.invalid", None)]),
        Upstream::default(),
    );
    let response = direct.http(&connect_request("nonexistent.invalid:443"));
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(
        body_of(&response),
        "agentrun: nonexistent.invalid:443 could not be resolved\n"
    );
    let response = direct.http("GET http://nonexistent.invalid/ HTTP/1.1\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 502"), "{response}");
    assert_eq!(
        body_of(&response),
        "agentrun: nonexistent.invalid:80 could not be resolved\n"
    );
    assert_eq!(
        direct.events(),
        vec![
            event("nonexistent.invalid", 443, None),
            event("nonexistent.invalid", 80, None),
        ]
    );
}

#[test]
fn upstream_refusing_connect_gets_502_with_its_status() {
    let port = start_upstream_answering(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        upstream_at(ProxyAddress {
            host: "127.0.0.1".to_string(),
            port,
            authorization: None,
        }),
    );
    let response = running.http(&connect_request("example.com:443"));
    assert!(
        response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "{response}"
    );
    assert_eq!(
        body_of(&response),
        "agentrun: example.com:443 upstream proxy answered 403\n"
    );
}

#[test]
fn upstream_closing_without_an_answer_gets_502() {
    let port = start_upstream_answering(b"");
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        upstream_at(ProxyAddress {
            host: "127.0.0.1".to_string(),
            port,
            authorization: None,
        }),
    );
    let response = running.http(&connect_request("example.com:443"));
    assert_eq!(
        body_of(&response),
        "agentrun: example.com:443 upstream proxy closed the connection\n"
    );
}

#[test]
fn unreachable_upstream_gets_502_without_its_address_or_credentials() {
    let port = closed_port();
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        upstream_at(ProxyAddress {
            host: "localhost".to_string(),
            port,
            authorization: Some(BASIC_USER_PASS.to_string()),
        }),
    );
    for (request, target) in [
        (connect_request("example.com:443"), "example.com:443"),
        (
            "GET http://example.com/ HTTP/1.1\r\n\r\n".to_string(),
            "example.com:80",
        ),
    ] {
        let response = running.http(&request);
        assert!(
            response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
            "{response}"
        );
        let body = body_of(&response);
        assert!(
            body.starts_with(&format!(
                "agentrun: {target} upstream proxy connection failed ("
            )),
            "{body}"
        );
        for secret in [
            "user",
            "pass",
            "dXNlcjpwYXNz",
            "localhost",
            "127.0.0.1",
            &port.to_string(),
        ] {
            assert!(!body.contains(secret), "{secret} in {body}");
        }
    }
}

#[test]
fn no_proxy_hosts_bypass_the_upstream() {
    let server = Server::start(false);
    let upstream = Server::start(true);
    let running = Running::start(
        custom(vec![rule("localhost", Some(server.port))]),
        Upstream {
            secure: Some(upstream.address()),
            plain: Some(upstream.address()),
            no_proxy: parse_no_proxy("localhost,127.0.0.0/8"),
        },
    );
    let response = running.http(&format!(
        "GET http://localhost:{}/ HTTP/1.1\r\nHost: localhost\r\n\r\n",
        server.port
    ));
    assert!(response.ends_with("ok"), "{response}");
    assert_eq!(server.requests().len(), 1);
    assert!(upstream.requests().is_empty());
}

#[test]
fn other_request_lines_get_400() {
    let running = Running::start(full(), Upstream::default());
    for request in [
        "GET /relative HTTP/1.1\r\nHost: x\r\n\r\n",
        "GET https://example.com/ HTTP/1.1\r\n\r\n",
        "CONNECT example.com HTTP/1.1\r\n\r\n",
        "nonsense\r\n\r\n",
    ] {
        let response = running.http(request);
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{request:?}: {response}"
        );
    }
    assert!(running.events().is_empty());
}

fn socks_handshake(stream: &mut TcpStream, methods: &[u8]) -> [u8; 2] {
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).unwrap();
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).unwrap();
    reply
}

fn socks_request(stream: &mut TcpStream, command: u8, address: &[u8], port: u16) -> Vec<u8> {
    let mut request = vec![0x05, command, 0x00];
    request.extend_from_slice(address);
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).unwrap();
    let mut reply = [0u8; 10];
    match stream.read_exact(&mut reply) {
        Ok(()) => reply.to_vec(),
        Err(_) => Vec::new(),
    }
}

fn domain(name: &str) -> Vec<u8> {
    let mut bytes = vec![0x03, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

#[test]
fn socks5_domain_and_ipv4_connect() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![
            rule("localhost", Some(server.port)),
            rule("127.0.0.1", Some(server.port)),
        ]),
        Upstream::default(),
    );
    let mut stream = running.tcp();
    assert_eq!(socks_handshake(&mut stream, &[0x00]), [0x05, 0x00]);
    let reply = socks_request(&mut stream, 0x01, &domain("localhost"), server.port);
    assert_eq!(reply, vec![0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    assert!(get_through_tunnel(&mut stream).ends_with("ok"));
    let mut stream = running.tcp();
    assert_eq!(socks_handshake(&mut stream, &[0x02, 0x00]), [0x05, 0x00]);
    let reply = socks_request(&mut stream, 0x01, &[0x01, 127, 0, 0, 1], server.port);
    assert_eq!(reply[1], 0x00);
    assert!(get_through_tunnel(&mut stream).ends_with("ok"));
    assert_eq!(server.requests().len(), 2);
    assert_eq!(
        running.events(),
        vec![
            event("localhost", server.port, None),
            event("127.0.0.1", server.port, None),
        ]
    );
}

#[test]
fn socks5_replies_for_denied_unsupported_and_ipv6() {
    let port = closed_port();
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(port))]),
        Upstream::default(),
    );
    let mut stream = running.tcp();
    socks_handshake(&mut stream, &[0x00]);
    assert_eq!(
        socks_request(&mut stream, 0x01, &domain("other.example"), 443)[1],
        0x02
    );
    let mut stream = running.tcp();
    socks_handshake(&mut stream, &[0x00]);
    assert_eq!(
        socks_request(&mut stream, 0x02, &domain("other.example"), 443)[1],
        0x07
    );
    let mut stream = running.tcp();
    socks_handshake(&mut stream, &[0x00]);
    assert_eq!(
        socks_request(
            &mut stream,
            0x01,
            &[0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            443
        )[1],
        0x08
    );
    let mut stream = running.tcp();
    socks_handshake(&mut stream, &[0x00]);
    assert_eq!(
        socks_request(&mut stream, 0x01, &[0x01, 127, 0, 0, 1], port)[1],
        0x05
    );
    let mut stream = running.tcp();
    assert_eq!(socks_handshake(&mut stream, &[0x02]), [0x05, 0xff]);
    assert_eq!(
        running.events(),
        vec![
            event("other.example", 443, Some(NetworkReason::NotAllowed)),
            event("127.0.0.1", port, None),
        ]
    );
}

#[test]
fn unix_socket_entry_works_like_the_tcp_one() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(server.port))]),
        Upstream::default(),
    );
    let mut stream = UnixStream::connect(&running.proxy.endpoint().socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(connect_request(&format!("127.0.0.1:{}", server.port)).as_bytes())
        .unwrap();
    assert!(read_head(&mut stream).starts_with("HTTP/1.1 200"));
    stream
        .write_all(b"GET /unix HTTP/1.1\r\nHost: inner\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(read_all(&mut stream).ends_with("ok"));
    assert!(server.requests()[0].starts_with("GET /unix "));
    let socket = running.proxy.endpoint().socket.clone();
    let port = running.proxy.endpoint().port.unwrap();
    drop(running);
    assert!(!socket.exists());
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[test]
fn shutdown_does_not_wait_when_the_socket_file_was_removed() {
    let running = Running::start(full(), Upstream::default());
    let socket = running.proxy.endpoint().socket.clone();
    std::fs::remove_file(&socket).unwrap();
    let started = std::time::Instant::now();
    drop(running);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(!socket.exists());
}

#[test]
fn plain_http_forwards_one_request_per_connection_with_its_body() {
    let server = Server::start(false);
    let upstream = Server::start(false);
    let listed = custom(vec![rule("127.0.0.1", Some(server.port))]);
    for (running, prefix) in [
        (
            Running::start(listed.clone(), Upstream::default()),
            String::new(),
        ),
        (
            Running::start(
                listed.clone(),
                Upstream {
                    secure: None,
                    plain: Some(upstream.address()),
                    no_proxy: Vec::new(),
                },
            ),
            format!("http://127.0.0.1:{}", server.port),
        ),
    ] {
        let mut stream = running.tcp();
        stream
            .write_all(
                format!(
                    "POST http://127.0.0.1:{port}/one HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 5\r\n\r\nhelloGET http://127.0.0.1:{port}/two HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
                    port = server.port
                )
                .as_bytes(),
            )
            .unwrap();
        let response = read_all(&mut stream);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
        let seen = if prefix.is_empty() {
            server.requests()
        } else {
            upstream.requests()
        };
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].starts_with(&format!(
                "POST {prefix}/one HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 5\r\nConnection: close\r\n",
                server.port
            )),
            "{}",
            seen[0]
        );
        assert!(seen[0].ends_with("\r\n\r\nhello"), "{}", seen[0]);
        assert!(!seen[0].contains("/two"), "{}", seen[0]);
        drop(running);
        server.requests.lock().unwrap().clear();
        upstream.requests.lock().unwrap().clear();
    }
}

const SLOW_RESPONSE: &str =
    "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world";

fn start_slow_server() -> (u16, Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, half_closed) = channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_head(&mut stream);
        thread::sleep(Duration::from_millis(100));
        stream.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        let _ = sender.send(matches!(stream.read(&mut byte), Ok(0)));
        stream.set_nonblocking(false).unwrap();
        let _ = stream.write_all(SLOW_RESPONSE.as_bytes());
        let _ = stream.shutdown(Shutdown::Both);
    });
    (port, half_closed)
}

fn get_and_half_close(running: &Running, url: &str) -> String {
    let mut stream = running.tcp();
    stream
        .write_all(format!("GET {url} HTTP/1.1\r\nHost: ignored\r\n\r\n").as_bytes())
        .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    read_all(&mut stream)
}

#[test]
fn half_closed_client_gets_the_direct_response() {
    let (port, _) = start_slow_server();
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(port))]),
        Upstream::default(),
    );
    let response = get_and_half_close(&running, &format!("http://127.0.0.1:{port}/"));
    assert_eq!(response, SLOW_RESPONSE);
}

#[test]
fn half_closed_client_gets_the_response_through_the_upstream_proxy() {
    let (port, half_closed) = start_slow_server();
    let running = Running::start(
        custom(vec![rule("example.com", None)]),
        Upstream {
            secure: None,
            plain: Some(ProxyAddress {
                host: "127.0.0.1".to_string(),
                port,
                authorization: None,
            }),
            no_proxy: Vec::new(),
        },
    );
    let response = get_and_half_close(&running, "http://example.com/");
    assert_eq!(response, SLOW_RESPONSE);
    assert_eq!(half_closed.recv_timeout(Duration::from_secs(5)), Ok(false));
}

#[test]
fn plain_http_rewrites_the_host_header_to_the_judged_target() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(server.port))]),
        Upstream::default(),
    );
    let response = running.http(&format!(
        "GET http://127.0.0.1:{}/ HTTP/1.1\r\nHost: other.example\r\nhost: another.example\r\n\r\n",
        server.port
    ));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(
        server.requests(),
        vec![format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            server.port
        )]
    );
}

#[test]
fn chunked_and_conflicting_request_bodies_are_refused() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![rule("127.0.0.1", Some(server.port))]),
        Upstream::default(),
    );
    let chunked = running.http(&format!(
        "POST http://127.0.0.1:{}/ HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        server.port
    ));
    assert!(
        chunked.starts_with("HTTP/1.1 411 Length Required\r\n"),
        "{chunked}"
    );
    let conflicting = running.http(&format!(
        "POST http://127.0.0.1:{}/ HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
        server.port
    ));
    assert!(
        conflicting.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{conflicting}"
    );
    let bad_version = running.http(&format!(
        "GET http://127.0.0.1:{}/ HTTP/2\r\n\r\n",
        server.port
    ));
    assert!(
        bad_version.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{bad_version}"
    );
    assert!(server.requests().is_empty());
    assert!(running.events().is_empty());
}

#[test]
fn hosts_with_invalid_characters_are_refused_without_events() {
    let running = Running::start(full(), Upstream::default());
    let connect = running.http("CONNECT bad\x01host.example:443 HTTP/1.1\r\n\r\n");
    assert!(
        connect.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{connect}"
    );
    let fragment = running.http("GET http://exa#mple.com/ HTTP/1.1\r\n\r\n");
    assert!(
        fragment.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{fragment}"
    );
    let space = running.http("GET http://exa\tmple.com/ HTTP/1.1\r\n\r\n");
    assert!(space.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{space}");
    let mut stream = running.tcp();
    socks_handshake(&mut stream, &[0x00]);
    assert_eq!(
        socks_request(&mut stream, 0x01, &domain("x\ny.example"), 443)[1],
        0x02
    );
    assert!(running.events().is_empty());
}

#[test]
fn wildcard_rules_do_not_skip_the_private_address_check() {
    let server = Server::start(false);
    let running = Running::start(
        custom(vec![HostRule {
            host: "0.0.1".to_string(),
            wildcard: true,
            port: Some(server.port),
        }]),
        Upstream::default(),
    );
    let response = running.http(&connect_request(&format!("127.0.0.1:{}", server.port)));
    assert!(
        response.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{response}"
    );
    assert_eq!(
        running.events(),
        vec![event(
            "127.0.0.1",
            server.port,
            Some(NetworkReason::PrivateAddress)
        )]
    );
    assert!(server.requests().is_empty());
}
