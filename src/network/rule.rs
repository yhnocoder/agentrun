use super::address::valid_host_name;
use crate::cli::NetworkMode;

const DEFAULT_PORTS: [u16; 2] = [443, 80];
const SERVICE_PORT: u16 = 443;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub mode: NetworkMode,
    pub rules: Vec<HostRule>,
    pub service_hosts: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Allowance {
    Listed,
    Service,
    Any,
}

impl Policy {
    pub(super) fn allowance(&self, host: &str, port: u16) -> Option<Allowance> {
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
}
