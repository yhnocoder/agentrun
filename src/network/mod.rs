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

pub use rule::HostRule;

pub(crate) use address::in_cidr;
pub(crate) use proxy::FilterProxy;
pub(crate) use rule::{Policy, check_usage};
pub(crate) use upstream::ProxyAddress;

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
mod tests;
