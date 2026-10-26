//! Kubernetes support for the ingress gateway.
//!
//! Two jobs, both through the Kubernetes API of a kcore cluster:
//!
//! 1. Resolve `kind: kubernetes` backends (a Service's load balancer address,
//!    a NodePort on the ready workers, or EndpointSlice pod addresses).
//! 2. Ingress controller mode: turn `Ingress` objects with
//!    `ingressClassName: kcore` into read-only routes and write the gateway
//!    address back to `status.loadBalancer`.
//!
//! The API is reached as a dedicated `kcore-ingress` ServiceAccount, never the
//! admin kubeconfig. Its ClusterRole can read Ingresses, Services,
//! EndpointSlices and Nodes, write `ingresses/status`, and create the
//! `IngressClass`. Tokens are minted with the TokenRequest API, live an hour,
//! and are held in memory only.
//!
//! Everything that parses or builds a document is a pure function with unit
//! tests; the async half only moves bytes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use aws_lc_rs::digest;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::db::Database;
use crate::ingress_spec::{
    Backend, GatewaySpec, K8sEndpoints, PathSpec, PortRef, RouteKind, RouteSpec, RouteWaf,
};
use crate::kubernetes_admin::kubernetes_request_as;
use crate::node_client::NodeClients;

pub const INGRESS_CLASS: &str = "kcore";
pub const CONTROLLER_NAME: &str = "kcore.io/ingress-controller";
const SA_NAMESPACE: &str = "kube-system";
const SA_NAME: &str = "kcore-ingress";
const TOKEN_TTL_SECS: u64 = 3600;
/// Mint a fresh token when the cached one is older than this.
const TOKEN_REUSE: Duration = Duration::from_secs(30 * 60);

pub const ANNOTATION_WAF_POLICY: &str = "ingress.kcore.io/waf-policy";
pub const ANNOTATION_WAF_MODE: &str = "ingress.kcore.io/waf-mode";
pub const ANNOTATION_RATE_LIMIT: &str = "ingress.kcore.io/rate-limit";

// ------------------------------------------------------------ RBAC documents

/// `(collection path, object path, manifest)` for everything the ServiceAccount
/// needs, in creation order.
pub fn rbac_manifests() -> Vec<(String, String, Value)> {
    vec![
        (
            format!("/api/v1/namespaces/{SA_NAMESPACE}/serviceaccounts"),
            format!("/api/v1/namespaces/{SA_NAMESPACE}/serviceaccounts/{SA_NAME}"),
            json!({
                "apiVersion": "v1", "kind": "ServiceAccount",
                "metadata": {"name": SA_NAME, "namespace": SA_NAMESPACE}
            }),
        ),
        (
            "/apis/rbac.authorization.k8s.io/v1/clusterroles".into(),
            format!("/apis/rbac.authorization.k8s.io/v1/clusterroles/{SA_NAME}"),
            json!({
                "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole",
                "metadata": {"name": SA_NAME},
                "rules": [
                    {"apiGroups": ["networking.k8s.io"], "resources": ["ingresses"],
                     "verbs": ["get", "list"]},
                    {"apiGroups": ["networking.k8s.io"], "resources": ["ingresses/status"],
                     "verbs": ["get", "patch", "update"]},
                    {"apiGroups": ["networking.k8s.io"], "resources": ["ingressclasses"],
                     "verbs": ["get", "list", "create"]},
                    {"apiGroups": [""], "resources": ["services", "nodes"],
                     "verbs": ["get", "list"]},
                    {"apiGroups": ["discovery.k8s.io"], "resources": ["endpointslices"],
                     "verbs": ["get", "list"]}
                ]
            }),
        ),
        (
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings".into(),
            format!("/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/{SA_NAME}"),
            json!({
                "apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRoleBinding",
                "metadata": {"name": SA_NAME},
                "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole",
                            "name": SA_NAME},
                "subjects": [{"kind": "ServiceAccount", "name": SA_NAME,
                              "namespace": SA_NAMESPACE}]
            }),
        ),
    ]
}

pub fn ingress_class_manifest() -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1", "kind": "IngressClass",
        "metadata": {"name": INGRESS_CLASS},
        "spec": {"controller": CONTROLLER_NAME}
    })
}

pub fn token_request_body() -> String {
    json!({
        "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest",
        "spec": {"expirationSeconds": TOKEN_TTL_SECS}
    })
    .to_string()
}

pub fn token_from_response(body: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("token response: {e}"))?;
    v["status"]["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "token response has no token".into())
}

// ----------------------------------------------------------- backend parsing

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePortInfo {
    pub name: String,
    pub port: u16,
    pub node_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceInfo {
    /// Addresses (IPs or hostnames) from `status.loadBalancer.ingress`.
    pub lb_addrs: Vec<String>,
    pub ports: Vec<ServicePortInfo>,
}

pub fn parse_service(body: &str) -> Result<ServiceInfo, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("service: {e}"))?;
    let lb_addrs = v["status"]["loadBalancer"]["ingress"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i["ip"].as_str().or_else(|| i["hostname"].as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let ports = v["spec"]["ports"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some(ServicePortInfo {
                        name: p["name"].as_str().unwrap_or_default().to_string(),
                        port: u16::try_from(p["port"].as_u64()?).ok()?,
                        node_port: p["nodePort"].as_u64().and_then(|n| u16::try_from(n).ok()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ServiceInfo { lb_addrs, ports })
}

fn pick_port<'a>(info: &'a ServiceInfo, port: &PortRef) -> Result<&'a ServicePortInfo, String> {
    info.ports
        .iter()
        .find(|p| match port {
            PortRef::Number(n) => p.port == *n,
            PortRef::Name(name) => &p.name == name,
        })
        .ok_or_else(|| format!("service has no port {port}"))
}

fn host_port(addr: &str, port: u16) -> String {
    if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

/// `loadBalancer` mode: the Service's external addresses on its Service port.
pub fn load_balancer_dials(info: &ServiceInfo, port: &PortRef) -> Result<Vec<String>, String> {
    let p = pick_port(info, port)?;
    if info.lb_addrs.is_empty() {
        return Err("service has no load balancer address yet".into());
    }
    Ok(info.lb_addrs.iter().map(|a| host_port(a, p.port)).collect())
}

/// `nodePort` mode: the Service's node port on every ready worker.
pub fn node_port_dials(
    info: &ServiceInfo,
    port: &PortRef,
    worker_ips: &[String],
) -> Result<Vec<String>, String> {
    let p = pick_port(info, port)?;
    let node_port = p
        .node_port
        .ok_or("service port has no nodePort; the Service type is not NodePort or LoadBalancer")?;
    if worker_ips.is_empty() {
        return Err("no ready worker nodes".into());
    }
    Ok(worker_ips
        .iter()
        .map(|ip| host_port(ip, node_port))
        .collect())
}

/// Ready, non-control-plane nodes and their InternalIP.
pub fn parse_worker_ips(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let mut ips = Vec::new();
    for n in v["items"].as_array().cloned().unwrap_or_default() {
        let labels = &n["metadata"]["labels"];
        if labels
            .get("node-role.kubernetes.io/control-plane")
            .is_some()
            || labels.get("node-role.kubernetes.io/master").is_some()
        {
            continue;
        }
        let ready = n["status"]["conditions"]
            .as_array()
            .map(|c| {
                c.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            })
            .unwrap_or(false);
        if !ready || n["spec"]["unschedulable"].as_bool().unwrap_or(false) {
            continue;
        }
        if let Some(ip) = n["status"]["addresses"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["type"] == "InternalIP"))
            .and_then(|x| x["address"].as_str())
        {
            ips.push(ip.to_string());
        }
    }
    ips.sort();
    ips
}

/// `pods` mode: ready endpoint addresses from an EndpointSlice list. The slice
/// port is matched on the Service port's name.
pub fn pod_dials(
    slices_body: &str,
    info: &ServiceInfo,
    port: &PortRef,
) -> Result<Vec<String>, String> {
    let want = pick_port(info, port)?;
    let v: Value = serde_json::from_str(slices_body).map_err(|e| format!("endpointslices: {e}"))?;
    let mut dials = Vec::new();
    for slice in v["items"].as_array().cloned().unwrap_or_default() {
        let slice_port = slice["ports"].as_array().and_then(|ps| {
            ps.iter()
                .find(|p| p["name"].as_str().unwrap_or_default() == want.name)
                .and_then(|p| p["port"].as_u64())
                .and_then(|n| u16::try_from(n).ok())
        });
        let Some(slice_port) = slice_port else {
            continue;
        };
        for e in slice["endpoints"].as_array().cloned().unwrap_or_default() {
            // An unset `ready` means ready.
            if e["conditions"]["ready"].as_bool() == Some(false) {
                continue;
            }
            for a in e["addresses"].as_array().cloned().unwrap_or_default() {
                if let Some(a) = a.as_str() {
                    dials.push(host_port(a, slice_port));
                }
            }
        }
    }
    dials.sort();
    dials.dedup();
    if dials.is_empty() {
        return Err("service has no ready endpoints".into());
    }
    Ok(dials)
}

// -------------------------------------------------------- Ingress -> routes

#[derive(Debug, Clone, PartialEq)]
pub struct SynthRoute {
    pub name: String,
    pub spec: RouteSpec,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Synthesised {
    /// `k8s:<cluster>/<namespace>/<ingress>`
    pub owner: String,
    pub routes: Vec<SynthRoute>,
    pub warnings: Vec<String>,
}

pub fn owner_of(cluster: &str, namespace: &str, name: &str) -> String {
    format!("k8s:{cluster}/{namespace}/{name}")
}

/// A route name made of the owner, lowercased and limited to the characters
/// resource names allow. Long names keep a hash of the owner so two long names
/// never collide.
pub fn route_name(owner: &str, index: usize) -> String {
    let base: String = owner
        .trim_start_matches("k8s:")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let suffix = if index == 0 {
        String::new()
    } else {
        format!("-r{index}")
    };
    let name = format!("k8s-{base}{suffix}");
    if name.len() <= 63 {
        return name;
    }
    let hash: String = digest::digest(&digest::SHA256, format!("{owner}#{index}").as_bytes())
        .as_ref()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}-{hash}", &name[..54])
}

/// Is this `Ingress` ours?
pub fn is_kcore_ingress(ing: &Value) -> bool {
    ing["spec"]["ingressClassName"].as_str() == Some(INGRESS_CLASS)
        || ing["metadata"]["annotations"]["kubernetes.io/ingress.class"].as_str()
            == Some(INGRESS_CLASS)
}

/// Converts one `Ingress` into routes: one per rule, because a route has one
/// set of paths for all its hosts.
pub fn ingress_to_routes(cluster: &str, gateway: &str, ing: &Value) -> Result<Synthesised, String> {
    let ns = ing["metadata"]["namespace"].as_str().unwrap_or("default");
    let name = ing["metadata"]["name"]
        .as_str()
        .ok_or("ingress has no name")?;
    let owner = owner_of(cluster, ns, name);
    let ann = &ing["metadata"]["annotations"];
    let mut out = Synthesised {
        owner: owner.clone(),
        ..Default::default()
    };
    if ing["spec"]["defaultBackend"].is_object() {
        out.warnings
            .push("spec.defaultBackend is ignored: a route needs a hostname".into());
    }
    if ing["spec"]["tls"].as_array().is_some_and(|t| !t.is_empty()) {
        out.warnings
            .push("spec.tls is ignored: the gateway obtains certificates from ACME".into());
    }
    let waf = RouteWaf {
        policy: ann[ANNOTATION_WAF_POLICY].as_str().map(str::to_string),
        mode: ann[ANNOTATION_WAF_MODE].as_str().map(str::to_string),
        exclusions: Vec::new(),
    };
    let rate_limit = match ann[ANNOTATION_RATE_LIMIT].as_str() {
        Some(r) => Some(r.trim().parse::<u32>().map_err(|_| {
            format!("{ANNOTATION_RATE_LIMIT} must be a number of requests per minute")
        })?),
        None => None,
    };
    let rules = ing["spec"]["rules"].as_array().cloned().unwrap_or_default();
    for (i, rule) in rules.iter().enumerate() {
        let host = rule["host"].as_str().unwrap_or_default().trim().to_string();
        if host.is_empty() {
            out.warnings
                .push(format!("rules[{i}] has no host and is skipped"));
            continue;
        }
        let mut paths: Vec<PathSpec> = Vec::new();
        for p in rule["http"]["paths"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let svc = &p["backend"]["service"];
            let Some(service) = svc["name"].as_str() else {
                return Err(format!("rules[{i}]: only Service backends are supported"));
            };
            let port = match (&svc["port"]["name"], &svc["port"]["number"]) {
                (Value::String(n), _) => PortRef::Name(n.clone()),
                (_, Value::Number(n)) => PortRef::Number(
                    n.as_u64()
                        .and_then(|n| u16::try_from(n).ok())
                        .ok_or_else(|| format!("rules[{i}]: invalid port number"))?,
                ),
                _ => return Err(format!("rules[{i}]: backend service has no port")),
            };
            let path = p["path"].as_str().filter(|s| !s.is_empty()).unwrap_or("/");
            let prefix = if path == "/" {
                "/".to_string()
            } else {
                path.trim_end_matches('/').to_string()
            };
            if p["pathType"].as_str() == Some("Exact") {
                out.warnings.push(format!(
                    "rules[{i}]: pathType Exact on {path} is served as a prefix"
                ));
            }
            if paths.iter().any(|x| x.prefix == prefix) {
                continue;
            }
            paths.push(PathSpec {
                prefix,
                backends: vec![Backend::Kubernetes {
                    cluster: cluster.to_string(),
                    namespace: ns.to_string(),
                    service: service.to_string(),
                    port,
                    endpoints: K8sEndpoints::LoadBalancer,
                }],
                load_balancing: Default::default(),
                health_check: None,
                backend_tls: None,
                proxy_protocol: false,
            });
        }
        if paths.is_empty() {
            out.warnings
                .push(format!("rules[{i}] ({host}) has no paths and is skipped"));
            continue;
        }
        out.routes.push(SynthRoute {
            name: route_name(&owner, out.routes.len()),
            spec: RouteSpec {
                gateway: gateway.to_string(),
                kind: RouteKind::Http,
                hosts: vec![host],
                paths,
                port: None,
                backends: Vec::new(),
                proxy_protocol: false,
                rate_limit,
                tls: Default::default(),
                waf: waf.clone(),
                headers: Default::default(),
                redirect_http: true,
            },
        });
    }
    if out.routes.is_empty() && out.warnings.is_empty() {
        out.warnings.push("ingress has no usable rules".into());
    }
    Ok(out)
}

pub fn status_patch_body(address: &str) -> String {
    let key = if address.parse::<std::net::IpAddr>().is_ok() {
        "ip"
    } else {
        "hostname"
    };
    json!({"status": {"loadBalancer": {"ingress": [{ key: address }]}}}).to_string()
}

/// True when the Ingress already advertises exactly `address`.
pub fn status_matches(ing: &Value, address: &str) -> bool {
    ing["status"]["loadBalancer"]["ingress"]
        .as_array()
        .map(|a| {
            a.len() == 1
                && (a[0]["ip"].as_str() == Some(address)
                    || a[0]["hostname"].as_str() == Some(address))
        })
        .unwrap_or(false)
}

// ------------------------------------------------------------------- async

/// Per-cluster API handle: the control-plane node to ask and a scoped token.
pub struct ClusterApi {
    clients: NodeClients,
    node_address: String,
    token: String,
}

struct CachedToken {
    token: String,
    minted: Instant,
}

/// Clusters whose RBAC has been applied by this process, and tokens in use.
static RBAC_DONE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static TOKENS: LazyLock<Mutex<HashMap<String, CachedToken>>> = LazyLock::new(Default::default);

impl ClusterApi {
    pub async fn get(&self, path: &str) -> Result<String, String> {
        let (status, body) = self.request("GET", path, "", "").await?;
        if status == 200 {
            Ok(body)
        } else {
            Err(format!("GET {path} returned {status}"))
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &str,
        content_type: &str,
    ) -> Result<(i32, String), String> {
        kubernetes_request_as(
            &self.clients,
            &self.node_address,
            method,
            path,
            body,
            content_type,
            &self.token,
        )
        .await
    }

    pub async fn patch_status(&self, ns: &str, name: &str, address: &str) -> Result<(), String> {
        let path = format!("/apis/networking.k8s.io/v1/namespaces/{ns}/ingresses/{name}/status");
        let (status, body) = self
            .request(
                "PATCH",
                &path,
                &status_patch_body(address),
                "application/merge-patch+json",
            )
            .await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("patching ingress status returned {status}: {body}"))
        }
    }

    pub async fn ensure_ingress_class(&self) -> Result<(), String> {
        let path = format!("/apis/networking.k8s.io/v1/ingressclasses/{INGRESS_CLASS}");
        if self.request("GET", &path, "", "").await?.0 == 200 {
            return Ok(());
        }
        let (status, body) = self
            .request(
                "POST",
                "/apis/networking.k8s.io/v1/ingressclasses",
                &ingress_class_manifest().to_string(),
                "",
            )
            .await?;
        if (200..300).contains(&status) || status == 409 {
            Ok(())
        } else {
            Err(format!(
                "creating the IngressClass returned {status}: {body}"
            ))
        }
    }
}

/// The address of a ready control-plane node of `cluster`.
fn control_plane_address(db: &Database, cluster: &str) -> Result<String, String> {
    let rows = db
        .list_kubernetes_cluster_nodes(cluster)
        .map_err(|e| e.to_string())?;
    for row in rows
        .iter()
        .filter(|r| r.role == "control-plane" && r.kubelet_ready)
    {
        if let Ok(Some(node)) = db.get_node(&row.node_id) {
            if node.status == "ready" {
                return Ok(node.address);
            }
        }
    }
    Err(format!("cluster {cluster} has no ready control-plane node"))
}

/// Applies the ServiceAccount, ClusterRole and binding with the admin
/// credentials: create when missing, replace the ClusterRole so its rules
/// follow this release.
async fn ensure_rbac(clients: &NodeClients, address: &str) -> Result<(), String> {
    for (collection, object, manifest) in rbac_manifests() {
        let (status, _) =
            kubernetes_request_as(clients, address, "GET", &object, "", "", "").await?;
        let (method, path) = match status {
            404 => ("POST", collection.as_str()),
            200 if manifest["kind"] == "ClusterRole" => ("PUT", object.as_str()),
            200 => continue,
            other => return Err(format!("GET {object} returned {other}")),
        };
        let (status, body) = kubernetes_request_as(
            clients,
            address,
            method,
            path,
            &manifest.to_string(),
            "",
            "",
        )
        .await?;
        if !(200..300).contains(&status) && status != 409 {
            return Err(format!("{method} {path} returned {status}: {body}"));
        }
    }
    Ok(())
}

/// A handle for `cluster`: RBAC applied once per process, token reused for 30
/// minutes.
pub async fn connect(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
) -> Result<ClusterApi, String> {
    let address = control_plane_address(db, cluster)?;
    let rbac_known = RBAC_DONE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(cluster);
    if !rbac_known {
        ensure_rbac(clients, &address).await?;
        RBAC_DONE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(cluster.to_string());
    }
    let cached = TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(cluster)
        .filter(|t| t.minted.elapsed() < TOKEN_REUSE)
        .map(|t| t.token.clone());
    let token = match cached {
        Some(t) => t,
        None => {
            let path = format!("/api/v1/namespaces/{SA_NAMESPACE}/serviceaccounts/{SA_NAME}/token");
            let (status, body) = kubernetes_request_as(
                clients,
                &address,
                "POST",
                &path,
                &token_request_body(),
                "",
                "",
            )
            .await?;
            if !(200..300).contains(&status) {
                // The RBAC may have been removed from under us.
                RBAC_DONE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(cluster);
                return Err(format!("minting the ingress token returned {status}"));
            }
            let t = token_from_response(&body)?;
            TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(
                cluster.to_string(),
                CachedToken {
                    token: t.clone(),
                    minted: Instant::now(),
                },
            );
            t
        }
    };
    Ok(ClusterApi {
        clients: clients.clone(),
        node_address: address,
        token,
    })
}

/// Resolves every Kubernetes backend in `backends` to dial addresses. A backend
/// that cannot be resolved maps to the reason.
pub async fn resolve_backends(
    db: &Database,
    clients: &NodeClients,
    backends: &[Backend],
) -> BTreeMap<String, Result<Vec<String>, String>> {
    let mut out = BTreeMap::new();
    let mut apis: HashMap<String, Result<ClusterApi, String>> = HashMap::new();
    for b in backends {
        let Backend::Kubernetes {
            cluster,
            namespace,
            service,
            port,
            endpoints,
        } = b
        else {
            continue;
        };
        if !apis.contains_key(cluster) {
            apis.insert(cluster.clone(), connect(db, clients, cluster).await);
        }
        let result = match &apis[cluster] {
            Err(e) => Err(e.clone()),
            Ok(api) => resolve_one(api, namespace, service, port, *endpoints).await,
        };
        out.insert(b.key(), result);
    }
    out
}

async fn resolve_one(
    api: &ClusterApi,
    ns: &str,
    svc: &str,
    port: &PortRef,
    mode: K8sEndpoints,
) -> Result<Vec<String>, String> {
    let info = parse_service(
        &api.get(&format!("/api/v1/namespaces/{ns}/services/{svc}"))
            .await?,
    )?;
    match mode {
        K8sEndpoints::LoadBalancer => load_balancer_dials(&info, port),
        K8sEndpoints::NodePort => {
            let nodes = api.get("/api/v1/nodes").await?;
            node_port_dials(&info, port, &parse_worker_ips(&nodes))
        }
        K8sEndpoints::Pods => {
            let slices = api
                .get(&format!(
                    "/apis/discovery.k8s.io/v1/namespaces/{ns}/endpointslices?labelSelector=kubernetes.io%2Fservice-name%3D{svc}"
                ))
                .await?;
            pod_dials(&slices, &info, port)
        }
    }
}

/// Ingress controller mode for one gateway: mirror the clusters' `kcore`
/// Ingresses into owner-tagged routes, drop routes whose Ingress is gone, and
/// write the gateway address into each Ingress's status.
pub async fn sync_ingresses(
    db: &Database,
    clients: &NodeClients,
    gateway_name: &str,
    gateway: &GatewaySpec,
) {
    let Some(k) = gateway.kubernetes.as_ref().filter(|k| k.ingress_controller) else {
        return;
    };
    let address = gateway
        .high_availability
        .as_ref()
        .map(|h| h.vip_address.clone())
        .or_else(|| gateway.listeners.first().map(|l| l.address.clone()))
        .unwrap_or_default();
    for cluster in &k.clusters {
        let prefix = format!("k8s:{cluster}/");
        let api = match connect(db, clients, cluster).await {
            Ok(api) => api,
            Err(e) => {
                warn!(gateway = %gateway_name, cluster = %cluster, error = %e, "ingress controller: cluster unreachable");
                continue;
            }
        };
        if let Err(e) = api.ensure_ingress_class().await {
            warn!(cluster = %cluster, error = %e, "ingress controller: IngressClass");
        }
        let body = match api.get("/apis/networking.k8s.io/v1/ingresses").await {
            Ok(b) => b,
            Err(e) => {
                warn!(cluster = %cluster, error = %e, "ingress controller: listing ingresses");
                continue;
            }
        };
        let Ok(list) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let mut wanted: HashSet<String> = HashSet::new();
        for ing in list["items"].as_array().cloned().unwrap_or_default() {
            if !is_kcore_ingress(&ing) {
                continue;
            }
            let ns = ing["metadata"]["namespace"]
                .as_str()
                .unwrap_or("default")
                .to_string();
            let name = ing["metadata"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let synth = match ingress_to_routes(cluster, gateway_name, &ing) {
                Ok(s) => s,
                Err(e) => {
                    warn!(ingress = %format!("{ns}/{name}"), error = %e, "ingress controller: skipping ingress");
                    continue;
                }
            };
            for w in &synth.warnings {
                debug!(ingress = %format!("{ns}/{name}"), warning = %w, "ingress controller");
            }
            for r in &synth.routes {
                wanted.insert(r.name.clone());
                if let Err(e) = upsert_synth_route(db, &synth.owner, r) {
                    warn!(route = %r.name, error = %e, "ingress controller: storing route");
                }
            }
            if !synth.routes.is_empty() && !address.is_empty() && !status_matches(&ing, &address) {
                if let Err(e) = api.patch_status(&ns, &name, &address).await {
                    warn!(ingress = %format!("{ns}/{name}"), error = %e, "ingress controller: status write-back");
                }
            }
        }
        // Routes whose Ingress (or rule) no longer exists.
        if let Ok(existing) = db.list_ingress_routes(gateway_name) {
            for row in existing.iter().filter(|r| r.owner.starts_with(&prefix)) {
                if !wanted.contains(&row.name) {
                    let _ = db.delete_ingress_route(&row.name);
                }
            }
        }
    }
}

fn upsert_synth_route(db: &Database, owner: &str, r: &SynthRoute) -> Result<(), String> {
    let spec_json = serde_json::to_string(&r.spec).map_err(|e| e.to_string())?;
    let existing = db.get_ingress_route(&r.name).map_err(|e| e.to_string())?;
    if let Some(e) = &existing {
        if !e.owner.is_empty() && e.owner != owner {
            return Err(format!("route {} belongs to {}", r.name, e.owner));
        }
        if e.owner.is_empty() {
            return Err(format!(
                "route {} exists and is not managed by Kubernetes",
                r.name
            ));
        }
        if e.spec_json == spec_json {
            return Ok(());
        }
    }
    db.upsert_ingress_route(&crate::db::IngressRouteRow {
        name: r.name.clone(),
        gateway: r.spec.gateway.clone(),
        generation: existing.as_ref().map(|e| e.generation + 1).unwrap_or(1),
        spec_json,
        owner: owner.to_string(),
        created_at: existing.map(|e| e.created_at).unwrap_or_default(),
        updated_at: String::new(),
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = r#"{"spec":{"ports":[
        {"name":"http","port":80,"nodePort":30080},{"name":"metrics","port":9090}]},
        "status":{"loadBalancer":{"ingress":[{"ip":"192.168.40.201"}]}}}"#;

    #[test]
    fn load_balancer_and_node_port_dials() {
        let info = parse_service(SERVICE).unwrap();
        assert_eq!(
            load_balancer_dials(&info, &PortRef::Name("http".into())).unwrap(),
            vec!["192.168.40.201:80"]
        );
        assert_eq!(
            load_balancer_dials(&info, &PortRef::Number(9090)).unwrap(),
            vec!["192.168.40.201:9090"]
        );
        assert!(load_balancer_dials(&info, &PortRef::Name("nope".into())).is_err());
        let ips = vec!["10.0.0.5".to_string(), "10.0.0.6".to_string()];
        assert_eq!(
            node_port_dials(&info, &PortRef::Name("http".into()), &ips).unwrap(),
            vec!["10.0.0.5:30080", "10.0.0.6:30080"]
        );
        // A ClusterIP-only port has no node port.
        assert!(node_port_dials(&info, &PortRef::Name("metrics".into()), &ips).is_err());
        let no_lb = parse_service(r#"{"spec":{"ports":[{"name":"a","port":1}]}}"#).unwrap();
        assert!(load_balancer_dials(&no_lb, &PortRef::Number(1))
            .unwrap_err()
            .contains("no load balancer address"));
    }

    #[test]
    fn workers_must_be_ready_schedulable_and_not_control_plane() {
        let nodes = r#"{"items":[
          {"metadata":{"labels":{}},"spec":{},
           "status":{"conditions":[{"type":"Ready","status":"True"}],
                     "addresses":[{"type":"InternalIP","address":"10.0.0.6"}]}},
          {"metadata":{"labels":{}},"spec":{},
           "status":{"conditions":[{"type":"Ready","status":"True"}],
                     "addresses":[{"type":"InternalIP","address":"10.0.0.5"}]}},
          {"metadata":{"labels":{"node-role.kubernetes.io/control-plane":""}},"spec":{},
           "status":{"conditions":[{"type":"Ready","status":"True"}],
                     "addresses":[{"type":"InternalIP","address":"10.0.0.1"}]}},
          {"metadata":{"labels":{}},"spec":{},
           "status":{"conditions":[{"type":"Ready","status":"False"}],
                     "addresses":[{"type":"InternalIP","address":"10.0.0.7"}]}},
          {"metadata":{"labels":{}},"spec":{"unschedulable":true},
           "status":{"conditions":[{"type":"Ready","status":"True"}],
                     "addresses":[{"type":"InternalIP","address":"10.0.0.8"}]}}]}"#;
        assert_eq!(parse_worker_ips(nodes), vec!["10.0.0.5", "10.0.0.6"]);
    }

    #[test]
    fn pod_dials_use_ready_endpoints_and_the_slice_port() {
        let info = parse_service(SERVICE).unwrap();
        let slices = r#"{"items":[{"ports":[{"name":"http","port":8080},{"name":"metrics","port":9091}],
          "endpoints":[{"addresses":["10.1.0.4"],"conditions":{"ready":true}},
                       {"addresses":["10.1.0.5"],"conditions":{"ready":false}},
                       {"addresses":["10.1.0.6"]}]}]}"#;
        assert_eq!(
            pod_dials(slices, &info, &PortRef::Name("http".into())).unwrap(),
            vec!["10.1.0.4:8080", "10.1.0.6:8080"]
        );
        assert!(pod_dials(r#"{"items":[]}"#, &info, &PortRef::Number(80)).is_err());
    }

    fn ingress(extra_annotations: &str) -> Value {
        serde_json::from_str(&format!(
            r#"{{"metadata":{{"name":"shop","namespace":"prod","annotations":{{{extra_annotations}}}}},
              "spec":{{"ingressClassName":"kcore","tls":[{{"secretName":"x"}}],
                "rules":[
                 {{"host":"shop.example.com","http":{{"paths":[
                    {{"path":"/","pathType":"Prefix","backend":{{"service":{{"name":"web","port":{{"number":80}}}}}}}},
                    {{"path":"/api/","pathType":"Exact","backend":{{"service":{{"name":"api","port":{{"name":"http"}}}}}}}}]}}}},
                 {{"host":"blog.example.com","http":{{"paths":[
                    {{"backend":{{"service":{{"name":"blog","port":{{"number":8080}}}}}}}}]}}}},
                 {{"http":{{"paths":[]}}}}]}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn an_ingress_becomes_one_route_per_host_rule() {
        let synth = ingress_to_routes(
            "lab-ha",
            "edge",
            &ingress(r#""ingress.kcore.io/waf-policy":"crs","ingress.kcore.io/waf-mode":"block","ingress.kcore.io/rate-limit":"120""#),
        )
        .unwrap();
        assert_eq!(synth.owner, "k8s:lab-ha/prod/shop");
        assert_eq!(synth.routes.len(), 2);
        let shop = &synth.routes[0];
        assert_eq!(shop.name, "k8s-lab-ha-prod-shop");
        assert_eq!(synth.routes[1].name, "k8s-lab-ha-prod-shop-r1");
        assert_eq!(shop.spec.hosts, vec!["shop.example.com"]);
        assert_eq!(shop.spec.waf.policy.as_deref(), Some("crs"));
        assert_eq!(shop.spec.waf.mode.as_deref(), Some("block"));
        assert_eq!(shop.spec.rate_limit, Some(120));
        let prefixes: Vec<&str> = shop.spec.paths.iter().map(|p| p.prefix.as_str()).collect();
        assert_eq!(prefixes, ["/", "/api"]);
        assert!(matches!(
            &shop.spec.paths[1].backends[0],
            Backend::Kubernetes { service, port: PortRef::Name(p), namespace, .. }
                if service == "api" && p == "http" && namespace == "prod"
        ));
        // The synthesised spec passes the same validation as an operator's.
        for r in &synth.routes {
            crate::ingress_spec::validate_route(&r.spec).unwrap();
        }
        // TLS secret, Exact path type and the host-less rule are all reported.
        let w = synth.warnings.join("|");
        assert!(w.contains("spec.tls is ignored") && w.contains("Exact") && w.contains("no host"));
    }

    #[test]
    fn bad_annotations_and_non_service_backends_are_refused() {
        assert!(ingress_to_routes(
            "c",
            "g",
            &ingress(r#""ingress.kcore.io/rate-limit":"lots""#)
        )
        .is_err());
        let resource: Value = serde_json::from_str(
            r#"{"metadata":{"name":"x"},"spec":{"rules":[{"host":"a.example.com","http":{"paths":[
               {"backend":{"resource":{"kind":"Bucket","name":"b"}}}]}}]}}"#,
        )
        .unwrap();
        assert!(ingress_to_routes("c", "g", &resource)
            .unwrap_err()
            .contains("Service"));
    }

    #[test]
    fn only_kcore_ingresses_are_ours() {
        assert!(is_kcore_ingress(&ingress("")));
        let nginx: Value =
            serde_json::from_str(r#"{"spec":{"ingressClassName":"nginx"}}"#).unwrap();
        assert!(!is_kcore_ingress(&nginx));
        let legacy: Value = serde_json::from_str(
            r#"{"metadata":{"annotations":{"kubernetes.io/ingress.class":"kcore"}}}"#,
        )
        .unwrap();
        assert!(is_kcore_ingress(&legacy));
    }

    #[test]
    fn route_names_are_valid_unique_and_bounded() {
        let a = route_name("k8s:lab/Prod_NS/Shop.v2", 0);
        assert_eq!(a, "k8s-lab-prod-ns-shop-v2");
        let long = "x".repeat(80);
        let n1 = route_name(&format!("k8s:c/ns/{long}a"), 0);
        let n2 = route_name(&format!("k8s:c/ns/{long}b"), 0);
        assert!(n1.len() <= 63 && n2.len() <= 63);
        assert_ne!(n1, n2);
        assert!(crate::grpc::validation::validate_network_name(&n1).is_ok());
    }

    #[test]
    fn status_write_back_is_idempotent() {
        assert_eq!(
            status_patch_body("203.0.113.10"),
            r#"{"status":{"loadBalancer":{"ingress":[{"ip":"203.0.113.10"}]}}}"#
        );
        assert!(status_patch_body("edge.example.com").contains("hostname"));
        let ing: Value = serde_json::from_str(
            r#"{"status":{"loadBalancer":{"ingress":[{"ip":"203.0.113.10"}]}}}"#,
        )
        .unwrap();
        assert!(status_matches(&ing, "203.0.113.10"));
        assert!(!status_matches(&ing, "203.0.113.11"));
        assert!(!status_matches(&Value::Null, "203.0.113.10"));
    }

    #[test]
    fn rbac_is_narrow() {
        let manifests = rbac_manifests();
        let role = &manifests[1].2;
        let text = role.to_string();
        // No secrets, no wildcards, no write access beyond ingress status.
        assert!(!text.contains("secrets") && !text.contains('*'));
        let writes: Vec<&Value> =
            role["rules"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| {
                    r["verbs"].as_array().unwrap().iter().any(|v| {
                        matches!(v.as_str(), Some("patch" | "update" | "create" | "delete"))
                    })
                })
                .collect();
        let resources: Vec<&str> = writes
            .iter()
            .flat_map(|r| r["resources"].as_array().unwrap())
            .filter_map(|r| r.as_str())
            .collect();
        assert_eq!(resources, ["ingresses/status", "ingressclasses"]);
    }

    #[test]
    fn token_responses_are_parsed() {
        assert_eq!(
            token_from_response(r#"{"status":{"token":"abc"}}"#).unwrap(),
            "abc"
        );
        assert!(token_from_response(r#"{"status":{}}"#).is_err());
        assert!(token_from_response("nope").is_err());
        assert!(token_request_body().contains("3600"));
    }
}
