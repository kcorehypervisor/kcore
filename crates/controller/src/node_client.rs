use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use tracing::info;

use crate::node_proto;

type ComputeClient = node_proto::node_compute_client::NodeComputeClient<NodeChannel>;
type AdminClient = node_proto::node_admin_client::NodeAdminClient<NodeChannel>;
type ContainerClient = node_proto::node_container_client::NodeContainerClient<NodeChannel>;
type StorageClient = node_proto::node_storage_client::NodeStorageClient<NodeChannel>;

/// How long agent call outcomes are kept for [`AgentCalls::count`].
const CALL_HISTORY: Duration = Duration::from_secs(3600);
const CALL_HISTORY_MAX: usize = 20_000;

/// When each call was made and whether the agent was at fault.
type CallHistory = VecDeque<(Instant, bool)>;

/// Outcomes of controller calls to each node agent, keyed by address.
/// The canary soak compares the canary's fault rate with its peers'.
#[derive(Clone, Default)]
pub struct AgentCalls {
    calls: Arc<Mutex<HashMap<String, CallHistory>>>,
}

/// Calls and faults to one agent inside a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallCount {
    pub calls: u32,
    pub faults: u32,
}

impl AgentCalls {
    pub fn record(&self, address: &str, fault: bool) {
        let now = Instant::now();
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        let history = calls.entry(address.to_string()).or_default();
        history.push_back((now, fault));
        while history.len() > CALL_HISTORY_MAX
            || history
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > CALL_HISTORY)
        {
            history.pop_front();
        }
    }

    pub fn count(&self, address: &str, since: Instant) -> CallCount {
        let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = CallCount::default();
        for (_, fault) in calls
            .get(address)
            .into_iter()
            .flatten()
            .filter(|(at, _)| *at >= since)
        {
            count.calls += 1;
            count.faults += u32::from(*fault);
        }
        count
    }
}

/// A gRPC status the agent returns when it, not the request, is at fault.
/// `InvalidArgument`, `NotFound`, `Unimplemented` and the like are answers.
pub fn is_agent_fault(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::Unknown
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Internal
            | tonic::Code::Unavailable
            | tonic::Code::DataLoss
    )
}

/// A [`Channel`] that records each call's outcome in [`AgentCalls`].
/// Errors in trailers after a streamed body are not seen.
#[derive(Clone)]
pub struct NodeChannel {
    inner: Channel,
    address: String,
    calls: AgentCalls,
}

type ChannelRequest = http::Request<tonic::body::BoxBody>;
type ChannelResponse = <Channel as tower::Service<ChannelRequest>>::Response;
type ChannelError = <Channel as tower::Service<ChannelRequest>>::Error;

impl tower::Service<ChannelRequest> for NodeChannel {
    type Response = ChannelResponse;
    type Error = ChannelError;
    type Future = Pin<Box<dyn Future<Output = Result<ChannelResponse, ChannelError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, request: ChannelRequest) -> Self::Future {
        let response = tower::Service::call(&mut self.inner, request);
        let address = self.address.clone();
        let calls = self.calls.clone();
        Box::pin(async move {
            let response = response.await;
            let fault = match &response {
                Err(_) => true,
                Ok(resp) => resp
                    .headers()
                    .get("grpc-status")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<i32>().ok())
                    .is_some_and(|code| is_agent_fault(tonic::Code::from(code))),
            };
            calls.record(&address, fault);
            response
        })
    }
}

#[derive(Clone)]
pub struct TlsClientConfig {
    pub ca_file: String,
    pub cert_file: String,
    pub key_file: String,
}

type ClientTuple = (ComputeClient, AdminClient, ContainerClient, StorageClient);

/// How long a new node dial may block. Live LAN nodes connect well inside this.
const NODE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone)]
pub struct NodeClients {
    clients: Arc<Mutex<HashMap<String, ClientTuple>>>,
    tls: Option<TlsClientConfig>,
    calls: AgentCalls,
}

impl NodeClients {
    pub fn new(tls: Option<TlsClientConfig>) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            tls,
            calls: AgentCalls::default(),
        }
    }

    pub fn agent_calls(&self) -> &AgentCalls {
        &self.calls
    }

    pub async fn connect(&self, address: &str) -> Result<()> {
        // Unreachable nodes must not stall dashboard pages. TCP's default
        // retransmit wait is several seconds, and overviews used to pay it
        // once per node.
        let channel = if let Some(tls_cfg) = &self.tls {
            let endpoint = format!("https://{address}");
            let ca_pem = std::fs::read_to_string(&tls_cfg.ca_file)?;
            let cert_pem = std::fs::read_to_string(&tls_cfg.cert_file)?;
            let key_pem = std::fs::read_to_string(&tls_cfg.key_file)?;
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem));
            Channel::from_shared(endpoint)?
                .connect_timeout(NODE_CONNECT_TIMEOUT)
                .tls_config(tls)?
                .connect()
                .await?
        } else {
            let endpoint = format!("http://{address}");
            Channel::from_shared(endpoint)?
                .connect_timeout(NODE_CONNECT_TIMEOUT)
                .connect()
                .await?
        };
        let channel = NodeChannel {
            inner: channel,
            address: address.to_string(),
            calls: self.calls.clone(),
        };
        let compute = ComputeClient::new(channel.clone());
        let admin = AdminClient::new(channel.clone());
        let container = ContainerClient::new(channel.clone());
        let storage = StorageClient::new(channel);

        info!(address, "connected to node");
        self.clients
            .lock()
            .unwrap()
            .insert(address.to_string(), (compute, admin, container, storage));
        Ok(())
    }

    pub fn get_compute(&self, address: &str) -> Option<ComputeClient> {
        self.clients
            .lock()
            .unwrap()
            .get(address)
            .map(|(c, _, _, _)| c.clone())
    }

    pub fn get_admin(&self, address: &str) -> Option<AdminClient> {
        self.clients
            .lock()
            .unwrap()
            .get(address)
            .map(|(_, a, _, _)| a.clone())
    }

    pub fn get_container(&self, address: &str) -> Option<ContainerClient> {
        self.clients
            .lock()
            .unwrap()
            .get(address)
            .map(|(_, _, c, _)| c.clone())
    }

    pub fn get_storage(&self, address: &str) -> Option<StorageClient> {
        self.clients
            .lock()
            .unwrap()
            .get(address)
            .map(|(_, _, _, s)| s.clone())
    }

    // Kubernetes-specific methods
    pub async fn apply_kubernetes_config(&self, _cluster_name: &str, _config: &str) -> Result<()> {
        // TODO: Implement actual config application via node agent
        Ok(())
    }

    pub async fn check_etcd_status(&self) -> Result<EtcdStatus> {
        // TODO: Implement actual etcd status check
        Ok(EtcdStatus {
            is_leader: true,
            is_healthy: true,
        })
    }

    pub async fn check_apiserver_status(&self) -> Result<ApiserverStatus> {
        // TODO: Implement actual apiserver status check
        Ok(ApiserverStatus { is_healthy: true })
    }

    pub async fn execute_command(&self, _command: &str) -> Result<String> {
        // TODO: Implement actual command execution
        Ok(String::new())
    }
}

#[derive(Debug, Clone)]
pub struct EtcdStatus {
    pub is_leader: bool,
    pub is_healthy: bool,
}

#[derive(Debug, Clone)]
pub struct ApiserverStatus {
    pub is_healthy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_calls_count_faults_inside_the_window() {
        let calls = AgentCalls::default();
        let start = Instant::now();
        calls.record("a:9091", false);
        calls.record("a:9091", true);
        calls.record("b:9091", false);
        assert_eq!(
            calls.count("a:9091", start),
            CallCount {
                calls: 2,
                faults: 1
            }
        );
        assert_eq!(calls.count("c:9091", start), CallCount::default());
        assert_eq!(
            calls.count("a:9091", Instant::now() + Duration::from_secs(1)),
            CallCount::default()
        );
    }

    #[test]
    fn only_server_side_codes_are_agent_faults() {
        assert!(is_agent_fault(tonic::Code::Internal));
        assert!(is_agent_fault(tonic::Code::Unavailable));
        assert!(!is_agent_fault(tonic::Code::InvalidArgument));
        assert!(!is_agent_fault(tonic::Code::Unimplemented));
        assert!(!is_agent_fault(tonic::Code::NotFound));
    }
}
