use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use tracing::info;

use crate::node_proto;

type ComputeClient = node_proto::node_compute_client::NodeComputeClient<Channel>;
type AdminClient = node_proto::node_admin_client::NodeAdminClient<Channel>;
type ContainerClient = node_proto::node_container_client::NodeContainerClient<Channel>;
type StorageClient = node_proto::node_storage_client::NodeStorageClient<Channel>;

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
}

impl NodeClients {
    pub fn new(tls: Option<TlsClientConfig>) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            tls,
        }
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
