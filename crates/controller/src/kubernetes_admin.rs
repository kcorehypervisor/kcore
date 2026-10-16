//! Kubernetes API calls made from a control-plane node's admin kubeconfig.
//!
//! Drain evicts pods. It does not delete a pod that is still running.

use std::time::{Duration, Instant};

use tokio::time::sleep;
use tonic::Request;

use crate::kubernetes_lifecycle::{
    match_registered_node, nodes_from_list, pods_from_list, pods_to_evict, ListedNode,
};
use crate::node_client::NodeClients;
use crate::node_proto;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn kubernetes_request(
    clients: &NodeClients,
    address: &str,
    method: &str,
    path: &str,
    body: &str,
    content_type: &str,
) -> Result<(i32, String), String> {
    if clients.get_admin(address).is_none() {
        clients
            .connect(address)
            .await
            .map_err(|err| format!("connect node {address}: {err}"))?;
    }
    let mut admin = clients
        .get_admin(address)
        .ok_or_else(|| format!("no admin client for {address}"))?;
    let response = admin
        .kubernetes_admin(Request::new(node_proto::KubernetesAdminRequest {
            method: method.to_string(),
            path: path.to_string(),
            body: body.to_string(),
            content_type: content_type.to_string(),
        }))
        .await
        .map_err(|err| format!("kubernetes admin {method} {path}: {err}"))?
        .into_inner();
    if !response.message.is_empty() && response.status_code == 0 {
        return Err(response.message);
    }
    Ok((response.status_code, response.body))
}

pub async fn list_nodes(clients: &NodeClients, address: &str) -> Result<Vec<ListedNode>, String> {
    let (status, body) =
        kubernetes_request(clients, address, "GET", "/api/v1/nodes", "", "").await?;
    if status != 200 {
        return Err(format!("list nodes returned {status}: {body}"));
    }
    Ok(nodes_from_list(&body))
}

pub fn registered_node_name(
    nodes: &[ListedNode],
    stored_name: &str,
    hostname: &str,
    ip: &str,
) -> Result<String, String> {
    match_registered_node(nodes, stored_name, hostname, ip)
        .map(|node| node.name.clone())
        .ok_or_else(|| format!("kubernetes has no node for hostname {hostname} address {ip}"))
}

pub fn node_is_ready(nodes: &[ListedNode], name: &str) -> bool {
    nodes
        .iter()
        .find(|node| node.name == name)
        .is_some_and(|node| node.ready)
}

pub async fn cordon(
    clients: &NodeClients,
    address: &str,
    name: &str,
    unschedulable: bool,
) -> Result<(), String> {
    let name = api_name(name)?;
    let body = format!(r#"{{"spec":{{"unschedulable":{unschedulable}}}}}"#);
    let (status, response) = kubernetes_request(
        clients,
        address,
        "PATCH",
        &format!("/api/v1/nodes/{name}"),
        &body,
        "application/merge-patch+json",
    )
    .await?;
    if status != 200 {
        return Err(format!("cordon {name} returned {status}: {response}"));
    }
    Ok(())
}

pub async fn delete_node(clients: &NodeClients, address: &str, name: &str) -> Result<(), String> {
    let name = api_name(name)?;
    let (status, body) = kubernetes_request(
        clients,
        address,
        "DELETE",
        &format!("/api/v1/nodes/{name}"),
        "",
        "",
    )
    .await?;
    if status != 200 && status != 202 && status != 404 {
        return Err(format!("delete node {name} returned {status}: {body}"));
    }
    Ok(())
}

/// Cordon, then evict pods until only DaemonSets and mirror pods remain.
/// A timeout uncordons the node and returns an error. Pods are not deleted.
pub async fn drain_node(clients: &NodeClients, address: &str, name: &str) -> Result<(), String> {
    cordon(clients, address, name, true).await?;
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        match remaining_pods(clients, address, name).await {
            Ok(pods) if pods.is_empty() => return Ok(()),
            Ok(pods) => {
                for (namespace, pod) in pods {
                    evict_pod(clients, address, &namespace, &pod).await?;
                }
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    let _ = cordon(clients, address, name, false).await;
                    return Err(format!("drain timed out: {err}"));
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = cordon(clients, address, name, false).await;
            return Err(format!(
                "drain timed out after {}s",
                DRAIN_TIMEOUT.as_secs()
            ));
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn remaining_pods(
    clients: &NodeClients,
    address: &str,
    name: &str,
) -> Result<Vec<(String, String)>, String> {
    let name = api_name(name)?;
    let path = format!("/api/v1/pods?fieldSelector=spec.nodeName%3D{name}");
    let (status, body) = kubernetes_request(clients, address, "GET", &path, "", "").await?;
    if status != 200 {
        return Err(format!("list pods returned {status}: {body}"));
    }
    Ok(pods_to_evict(&pods_from_list(&body)))
}

async fn evict_pod(
    clients: &NodeClients,
    address: &str,
    namespace: &str,
    name: &str,
) -> Result<(), String> {
    let namespace = api_name(namespace)?;
    let name = api_name(name)?;
    let body = format!(
        r#"{{"apiVersion":"policy/v1","kind":"Eviction","metadata":{{"name":"{name}","namespace":"{namespace}"}}}}"#
    );
    let path = format!("/api/v1/namespaces/{namespace}/pods/{name}/eviction");
    let (status, response) =
        kubernetes_request(clients, address, "POST", &path, &body, "application/json").await?;
    if matches!(status, 200 | 201 | 202 | 404 | 429) {
        return Ok(());
    }
    Err(format!(
        "evict {namespace}/{name} returned {status}: {response}"
    ))
}

fn api_name(name: &str) -> Result<String, String> {
    if name.is_empty()
        || name.contains('/')
        || name.contains("..")
        || name.contains(' ')
        || name.contains('?')
    {
        return Err(format!("invalid kubernetes name {name}"));
    }
    Ok(name.to_string())
}
