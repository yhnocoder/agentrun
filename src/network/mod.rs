mod address;
mod http;
mod proxy;
mod rule;
mod socks;
mod upstream;

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use crate::cli::{NetworkMode, Runtime};

pub use proxy::FilterProxy;
pub use rule::{HostRule, Policy};
pub use upstream::{ProxyAddress, Upstream, parse_no_proxy};

pub(crate) use rule::check_usage;

pub(crate) const SOCKET_FILE: &str = "proxy.sock";
pub(crate) const PORT_PLACEHOLDER: &str = "<proxy port>";
const PROXY_VARIABLES: [&str; 6] = [
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
];
const NO_PROXY_VARIABLES: [&str; 2] = ["NO_PROXY", "no_proxy"];
const LISTEN_ADDRESS: Ipv4Addr = Ipv4Addr::LOCALHOST;

pub(crate) fn proxy_needed(runtime: Runtime, mode: NetworkMode, sandboxed: bool) -> bool {
    sandboxed
        && match runtime {
            Runtime::Pi => true,
            Runtime::ClaudeCode => mode != NetworkMode::None,
            Runtime::Codex => mode == NetworkMode::Custom,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_runs_only_with_a_sandbox_and_depends_on_the_runtime() {
        for mode in [NetworkMode::None, NetworkMode::Full, NetworkMode::Custom] {
            assert!(proxy_needed(Runtime::Pi, mode, true), "{mode:?}");
            assert!(!proxy_needed(Runtime::Pi, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::ClaudeCode, mode, false), "{mode:?}");
            assert!(!proxy_needed(Runtime::Codex, mode, false), "{mode:?}");
        }
        assert!(!proxy_needed(Runtime::ClaudeCode, NetworkMode::None, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Full, true));
        assert!(proxy_needed(Runtime::ClaudeCode, NetworkMode::Custom, true));
        assert!(!proxy_needed(Runtime::Codex, NetworkMode::None, true));
        assert!(!proxy_needed(Runtime::Codex, NetworkMode::Full, true));
        assert!(proxy_needed(Runtime::Codex, NetworkMode::Custom, true));
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
}
