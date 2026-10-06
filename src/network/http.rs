use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::thread;

use super::address::{HTTP_DEFAULT_PORT, Target, authority_text, parse_host_port, target_text};
use super::proxy::{Client, Route, Server, admit, connect, read_head, relay, write_all};
use crate::output::NetworkReason;

const UNRESOLVED: &str = "could not be resolved";

pub(super) fn handle_http(mut client: Client, server: &Server, first: u8) {
    let Ok((head, leftover)) = read_head(&mut client, vec![first]) else {
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
        let connected = match route {
            None => Err(UNRESOLVED.to_string()),
            Some(route) => connect(&route, &target, true),
        };
        match connected {
            Err(reason) => {
                respond(
                    &mut client,
                    "502 Bad Gateway",
                    &failed_body(&target, &reason),
                );
            }
            Ok((stream, early)) => {
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
            respond(
                &mut client,
                "502 Bad Gateway",
                &failed_body(&target, UNRESOLVED),
            );
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
        Err(reason) => respond(
            &mut client,
            "502 Bad Gateway",
            &failed_body(&target, &reason),
        ),
        Ok((stream, _)) => {
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
    let _ = downstream.join();
    let _ = client.shutdown(Shutdown::Both);
    let _ = server_stream.shutdown(Shutdown::Both);
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

fn failed_body(target: &Target, reason: &str) -> String {
    format!("agentrun: {} {reason}\n", target_text(target))
}

fn denied_body(target: &Target, reason: NetworkReason) -> String {
    format!(
        "agentrun: {} is not allowed ({})\n",
        target_text(target),
        reason.name()
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_urls_and_header_names() {
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
    }
}
