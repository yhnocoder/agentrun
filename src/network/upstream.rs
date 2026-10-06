use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::net::Ipv4Addr;

use super::NO_PROXY_VARIABLES;
use super::address::{HTTP_DEFAULT_PORT, in_cidr};

const SECURE_UPSTREAM_VARIABLES: [&str; 4] =
    ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
const PLAIN_UPSTREAM_VARIABLES: [&str; 4] = ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyAddress {
    pub host: String,
    pub port: u16,
    pub authorization: Option<String>,
}

impl ProxyAddress {
    pub fn parse(value: &str) -> Option<ProxyAddress> {
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
    pub(super) fn select(
        &self,
        secure: bool,
        host: &str,
        addresses: &[Ipv4Addr],
    ) -> Option<&ProxyAddress> {
        let candidate = if secure { &self.secure } else { &self.plain };
        candidate.as_ref().filter(|_| {
            !self
                .no_proxy
                .iter()
                .any(|rule| rule.matches(host, addresses))
        })
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
}
