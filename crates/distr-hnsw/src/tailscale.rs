//! Tailscale LocalAPI client and bind discipline (DESIGN §3;
//! docs/m2-implementation-plan.md, decisions 1 and 2).
//!
//! The client speaks HTTP/1.1 to the host `tailscaled` over its Unix socket.
//! Daemons bind only to an address LocalAPI reports as this node's own:
//! wildcards are always refused, and loopback needs an explicit insecure
//! flag for tests and single-host development.

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use http_body_util::{BodyExt, Empty, Limited};
use hyper::{body::Bytes, header::HOST, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use thiserror::Error;
use tokio::net::{TcpListener, UnixStream};

pub const DEFAULT_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";
/// Overrides the LocalAPI socket path (tests point it at a fake LocalAPI).
pub const SOCKET_ENV: &str = "DISTR_HNSW_TAILSCALE_SOCKET";
/// Exit status of a daemon whose tailnet address changed or vanished. Its
/// supervisor restarts it, and the restart binds the new address
/// (`EX_TEMPFAIL`).
pub const EXIT_TAILNET_ADDRESS_CHANGED: i32 = 75;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// A certificate request may wait for an ACME issuance.
const CERT_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_RESPONSE_BYTES: usize = 4 << 20;
/// LocalAPI rejects requests without this host name.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";

#[derive(Debug, Error)]
pub enum LocalApiError {
    #[error("cannot reach tailscaled at {path}: {source}")]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("LocalAPI request failed: {0}")]
    Http(#[from] hyper::Error),
    #[error("LocalAPI {path} returned {status}")]
    Status { path: String, status: StatusCode },
    #[error("LocalAPI response is malformed: {0}")]
    Malformed(String),
    #[error("LocalAPI request timed out")]
    Timeout,
    #[error("{0} is not a tailnet peer")]
    UnknownPeer(SocketAddr),
}

/// This node as LocalAPI reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfNode {
    pub stable_id: String,
    pub addresses: Vec<IpAddr>,
    /// Domains LocalAPI can issue certificates for (empty when the tailnet
    /// has HTTPS certificates turned off).
    pub cert_domains: Vec<String>,
    /// `tailscaled` is logged in and connected (`BackendState` is `Running`).
    pub running: bool,
}

/// The tailnet node and user behind a socket address, from `WhoIs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub node_stable_id: String,
    pub node_name: String,
    pub user_login: String,
    pub tags: Vec<String>,
}

impl PeerIdentity {
    /// A tagged node acts as a service, not as a user: its user profile is
    /// the shared `tagged-devices` placeholder.
    pub fn is_tagged(&self) -> bool {
        !self.tags.is_empty()
    }
}

#[derive(Deserialize)]
struct StatusWire {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<StatusSelfWire>,
    #[serde(rename = "CertDomains", default)]
    cert_domains: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct StatusSelfWire {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<IpAddr>,
}

#[derive(Deserialize)]
struct WhoIsWire {
    #[serde(rename = "Node")]
    node: WhoIsNodeWire,
    #[serde(rename = "UserProfile")]
    user: WhoIsUserWire,
}

#[derive(Deserialize)]
struct WhoIsNodeWire {
    #[serde(rename = "StableID")]
    stable_id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct WhoIsUserWire {
    #[serde(rename = "LoginName")]
    login_name: String,
}

/// A LocalAPI client bound to one socket path.
#[derive(Clone, Debug)]
pub struct LocalApi {
    socket: PathBuf,
}

impl LocalApi {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// The socket named by `DISTR_HNSW_TAILSCALE_SOCKET`, else the default.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os(SOCKET_ENV)
                .map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from),
        )
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn status(&self) -> Result<SelfNode, LocalApiError> {
        let body = self.get("/localapi/v0/status").await?;
        let wire: StatusWire = serde_json::from_slice(&body)
            .map_err(|error| LocalApiError::Malformed(error.to_string()))?;
        let running = wire.backend_state == "Running";
        let cert_domains = wire.cert_domains.unwrap_or_default();
        let Some(node) = wire.self_node else {
            return Ok(SelfNode {
                stable_id: String::new(),
                addresses: Vec::new(),
                cert_domains,
                running: false,
            });
        };
        Ok(SelfNode {
            stable_id: node.id,
            addresses: node.tailscale_ips,
            cert_domains,
            running,
        })
    }

    /// Resolve the peer behind `addr` (the remote address of an accepted
    /// connection). An address no tailnet node owns is `UnknownPeer`.
    pub async fn whois(&self, addr: SocketAddr) -> Result<PeerIdentity, LocalApiError> {
        let path = format!("/localapi/v0/whois?addr={addr}");
        let body = match self.get(&path).await {
            Err(LocalApiError::Status { status, .. }) if status == StatusCode::NOT_FOUND => {
                return Err(LocalApiError::UnknownPeer(addr));
            }
            other => other?,
        };
        let wire: WhoIsWire = serde_json::from_slice(&body)
            .map_err(|error| LocalApiError::Malformed(error.to_string()))?;
        if wire.node.stable_id.is_empty() {
            return Err(LocalApiError::Malformed(
                "WhoIs node has no stable id".into(),
            ));
        }
        Ok(PeerIdentity {
            node_stable_id: wire.node.stable_id,
            node_name: wire.node.name,
            user_login: wire.user.login_name,
            tags: wire.node.tags.unwrap_or_default(),
        })
    }

    /// This node's certificate and private key for `domain`, as PEM: the
    /// key block first, then the certificate chain. `tailscaled` returns its
    /// cached certificate while it is fresh and renews it otherwise. Needs
    /// root or the tailnet operator user (403 otherwise).
    pub async fn cert_pair(&self, domain: &str) -> Result<Bytes, LocalApiError> {
        if domain.is_empty()
            || !domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        {
            return Err(LocalApiError::Malformed(format!(
                "invalid certificate domain {domain:?}"
            )));
        }
        let path = format!("/localapi/v0/cert/{domain}?type=pair");
        tokio::time::timeout(CERT_TIMEOUT, self.get_inner(&path))
            .await
            .map_err(|_| LocalApiError::Timeout)?
    }

    async fn get(&self, path: &str) -> Result<Bytes, LocalApiError> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.get_inner(path))
            .await
            .map_err(|_| LocalApiError::Timeout)?
    }

    async fn get_inner(&self, path: &str) -> Result<Bytes, LocalApiError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| LocalApiError::Connect {
                    path: self.socket.clone(),
                    source,
                })?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let request = Request::get(path)
            .header(HOST, LOCALAPI_HOST)
            .body(Empty::<Bytes>::new())
            .map_err(|error| LocalApiError::Malformed(error.to_string()))?;
        let response = sender.send_request(request).await?;
        if response.status() != StatusCode::OK {
            return Err(LocalApiError::Status {
                path: path.to_owned(),
                status: response.status(),
            });
        }
        let body = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|error| LocalApiError::Malformed(error.to_string()))?;
        Ok(body.to_bytes())
    }
}

/// Where a daemon may listen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindSpec {
    /// This node's tailnet IPv4 address, discovered through LocalAPI.
    Tailnet { port: u16 },
    /// An explicit address, which must be one of this node's tailnet
    /// addresses (or loopback with `--insecure-loopback`).
    Address(SocketAddr),
}

impl FromStr for BindSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some(port) = value.strip_prefix("tailnet:") {
            let port = port
                .parse()
                .map_err(|_| format!("invalid port in {value:?}"))?;
            return Ok(Self::Tailnet { port });
        }
        value
            .parse()
            .map(Self::Address)
            .map_err(|_| format!("expected tailnet:<port> or <ip>:<port>, got {value:?}"))
    }
}

#[derive(Debug, Error)]
pub enum BindError {
    #[error("refusing to bind the wildcard address {0}; bind a tailnet address")]
    Wildcard(SocketAddr),
    #[error("refusing to bind loopback {0} without --insecure-loopback")]
    LoopbackNotAllowed(SocketAddr),
    #[error("{0} is not one of this node's tailnet addresses")]
    NotTailnet(SocketAddr),
    #[error("this node has no tailnet IPv4 address")]
    NoTailnetAddress,
    #[error("tailscaled was not ready within {0:?}: {1}")]
    NotReady(Duration, String),
}

/// How a daemon ended up bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindMode {
    /// Bound to this node's tailnet address; peers are tailnet nodes.
    Tailnet,
    /// Bound to loopback by explicit operator choice; no tailnet identity.
    InsecureLoopback,
}

/// Decide the address to bind. Wildcards are refused outright; loopback is
/// allowed only when `insecure_loopback` is set and needs no LocalAPI; any
/// other address must be one LocalAPI reports for this node. Waits up to
/// `wait` for `tailscaled` to be running, retrying with backoff.
pub async fn resolve_bind(
    spec: BindSpec,
    insecure_loopback: bool,
    api: &LocalApi,
    wait: Duration,
) -> Result<(SocketAddr, BindMode), BindError> {
    if let BindSpec::Address(addr) = spec {
        if addr.ip().is_unspecified() {
            return Err(BindError::Wildcard(addr));
        }
        if addr.ip().is_loopback() {
            return if insecure_loopback {
                Ok((addr, BindMode::InsecureLoopback))
            } else {
                Err(BindError::LoopbackNotAllowed(addr))
            };
        }
    }
    let node = wait_until_running(api, wait).await?;
    match spec {
        BindSpec::Tailnet { port } => node
            .addresses
            .iter()
            .find(|ip| ip.is_ipv4())
            .map(|ip| (SocketAddr::new(*ip, port), BindMode::Tailnet))
            .ok_or(BindError::NoTailnetAddress),
        BindSpec::Address(addr) if node.addresses.contains(&addr.ip()) => {
            Ok((addr, BindMode::Tailnet))
        }
        BindSpec::Address(addr) => Err(BindError::NotTailnet(addr)),
    }
}

async fn wait_until_running(api: &LocalApi, wait: Duration) -> Result<SelfNode, BindError> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut backoff = Duration::from_millis(100);
    loop {
        let last = match api.status().await {
            Ok(node) if node.running && !node.addresses.is_empty() => return Ok(node),
            Ok(_) => "tailscaled is not running".to_owned(),
            Err(error) => error.to_string(),
        };
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(BindError::NotReady(wait, last));
        }
        tokio::time::sleep(backoff.min(deadline - now)).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

/// Bind a listener after `resolve_bind`.
pub async fn bind_listener(
    spec: BindSpec,
    insecure_loopback: bool,
    api: &LocalApi,
    wait: Duration,
) -> anyhow::Result<(TcpListener, BindMode)> {
    let (addr, mode) = resolve_bind(spec, insecure_loopback, api, wait).await?;
    Ok((TcpListener::bind(addr).await?, mode))
}

/// Resolves when this node no longer owns `bound` according to LocalAPI.
/// A LocalAPI that is briefly unreachable (`tailscaled` restarting) is not
/// a change; only a running node that reports other addresses is.
pub async fn address_lost(api: LocalApi, bound: IpAddr, poll: Duration) -> IpAddr {
    loop {
        tokio::time::sleep(poll).await;
        if let Ok(node) = api.status().await {
            if node.running && !node.addresses.contains(&bound) {
                return bound;
            }
        }
    }
}
