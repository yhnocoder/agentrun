use std::net::Ipv4Addr;

pub(super) const HTTP_DEFAULT_PORT: u16 = 80;
const HOST_NAME_LIMIT: usize = 253;
const PRIVATE_RANGES: [(Ipv4Addr, u8); 7] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
];

pub(super) fn is_private(address: Ipv4Addr) -> bool {
    PRIVATE_RANGES
        .iter()
        .any(|(network, bits)| in_cidr(address, *network, *bits))
}

pub(super) fn in_cidr(address: Ipv4Addr, network: Ipv4Addr, bits: u8) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(bits))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

pub(super) struct Target {
    pub(super) host: String,
    pub(super) port: u16,
}

pub(super) fn parse_host_port(authority: &str, default_port: Option<u16>) -> Option<Target> {
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

pub(super) fn normalize_host(host: &str) -> Option<String> {
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

pub(super) fn valid_host_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= HOST_NAME_LIMIT
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(super) fn target_text(target: &Target) -> String {
    format!("{}:{}", target.host, target.port)
}

pub(super) fn authority_text(target: &Target) -> String {
    if target.port == HTTP_DEFAULT_PORT {
        target.host.clone()
    } else {
        target_text(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn host_ports_and_names_normalise() {
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
    }
}
