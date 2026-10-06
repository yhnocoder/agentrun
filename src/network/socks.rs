use std::io::{Read, Write};
use std::net::Ipv4Addr;

use super::address::{Target, normalize_host};
use super::proxy::{Client, Decision, Server, admit, connect, relay, write_all};

pub(super) const SOCKS_VERSION: u8 = 0x05;
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

pub(super) fn handle_socks(mut client: Client, server: &Server) {
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
        Decision::Denied(_) => {
            let _ = client.write_all(&socks_reply(SOCKS_NOT_ALLOWED));
            return;
        }
        Decision::Route(route) => route,
        Decision::Unreachable => {
            let _ = client.write_all(&socks_reply(SOCKS_HOST_UNREACHABLE));
            return;
        }
    };
    match connect(&route, &target, true) {
        Err(_) => {
            let _ = client.write_all(&socks_reply(SOCKS_CONNECTION_REFUSED));
        }
        Ok((stream, early)) => {
            if write_all(&mut client, &socks_reply(SOCKS_SUCCEEDED))
                && write_all(&mut client, &early)
            {
                relay(client, stream, &[]);
            }
        }
    }
}

fn read_byte(stream: &mut impl Read) -> Option<u8> {
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).ok().map(|_| byte[0])
}

fn socks_reply(code: u8) -> [u8; 10] {
    [SOCKS_VERSION, code, 0, SOCKS_ATYP_IPV4, 0, 0, 0, 0, 0, 0]
}
