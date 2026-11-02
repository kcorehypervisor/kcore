//! gRPC adapter over the catalog.

// tonic::Status is larger than Clippy's Result error budget.
#![allow(clippy::result_large_err)]

use crate::apply::{self, Layout};
use crate::model::{AddressPool, Catalog, DnsRecord, Network, Reservation};
use crate::proto::network_provisioner_server::{NetworkProvisioner, NetworkProvisionerServer};
use crate::proto::{
    ApplyRequest, ApplyResponse, DeleteNetworkRequest, DeleteNetworkResponse, DeleteRecordRequest,
    DeleteRecordResponse, DeleteReservationRequest, DeleteReservationResponse,
    DnsRecord as ProtoRecord, GetNetworkRequest, GetStatusRequest, ImportRouterOsRequest,
    ImportRouterOsResponse, ListNetworksRequest, ListNetworksResponse, ListRecordsRequest,
    ListRecordsResponse, ListReservationsRequest, ListReservationsResponse,
    Network as ProtoNetwork, PutNetworkRequest, Reservation as ProtoReservation, Status,
};
use crate::routeros;
use crate::store::Store;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status as RpcStatus};
use tracing::info;

pub struct Engine {
    pub store: Store,
    pub layout: Layout,
    pub token: String,
    pub sync_dns: bool,
    pub reload: bool,
}

impl Engine {
    pub fn open(layout: Layout, sync_dns: bool, reload: bool) -> anyhow::Result<Self> {
        let store = Store::open(&layout.database())?;
        let token = apply::ensure_secret(&layout.token())?;
        Ok(Self {
            store,
            layout,
            token,
            sync_dns,
            reload,
        })
    }

    pub fn apply(&mut self) -> anyhow::Result<ApplyResponse> {
        let catalog = self.store.load()?;
        catalog.validate().map_err(|err| anyhow::anyhow!(err))?;
        let rendered = apply::render(&self.layout, &catalog)?;
        let mut notes = Vec::new();
        if self.sync_dns {
            let zones = crate::render::zone_plan(&catalog);
            match apply::sync_dns(&self.layout, &zones) {
                Ok(()) => notes.push("powerdns updated".into()),
                Err(err) => notes.push(format!("powerdns sync failed: {err}")),
            }
        }
        if self.reload {
            notes.extend(apply::reload_services());
        }
        Ok(ApplyResponse {
            kea_config_path: rendered.kea_path.display().to_string(),
            zones: rendered.zones,
            message: notes.join("; "),
        })
    }
}

#[derive(Clone)]
pub struct Service {
    engine: Arc<Mutex<Engine>>,
}

impl Service {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine: Arc::new(Mutex::new(engine)),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Engine>, RpcStatus> {
        self.engine
            .lock()
            .map_err(|_| RpcStatus::internal("provisioning lock poisoned"))
    }

    fn auth<T>(&self, request: &Request<T>) -> Result<(), RpcStatus> {
        let token = self.lock()?.token.clone();
        if token.is_empty() {
            return Ok(());
        }
        let header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let presented = header.strip_prefix("Bearer ").unwrap_or(header);
        if presented == token {
            Ok(())
        } else {
            Err(RpcStatus::unauthenticated("bearer token required"))
        }
    }
}

pub async fn serve(service: Service, addrs: Vec<SocketAddr>) -> anyhow::Result<()> {
    if addrs.is_empty() {
        anyhow::bail!("no listen address");
    }
    let mut tasks = tokio::task::JoinSet::new();
    for addr in addrs {
        let svc = NetworkProvisionerServer::new(service.clone());
        info!(%addr, "network provisioner listening");
        tasks.spawn(async move {
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve(addr)
                .await
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

#[tonic::async_trait]
impl NetworkProvisioner for Service {
    async fn put_network(
        &self,
        request: Request<PutNetworkRequest>,
    ) -> Result<Response<ProtoNetwork>, RpcStatus> {
        self.auth(&request)?;
        let proto = request
            .into_inner()
            .network
            .ok_or_else(|| RpcStatus::invalid_argument("network is required"))?;
        let network = network_from_proto(&proto)?;
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        if let Some(slot) = catalog.networks.iter_mut().find(|n| n.name == network.name) {
            *slot = network.clone();
        } else {
            catalog.networks.push(network.clone());
        }
        save(&mut engine, &catalog)?;
        Ok(Response::new(network_to_proto(&network)))
    }

    async fn get_network(
        &self,
        request: Request<GetNetworkRequest>,
    ) -> Result<Response<ProtoNetwork>, RpcStatus> {
        self.auth(&request)?;
        let name = request.into_inner().name;
        let engine = self.lock()?;
        let catalog = load(&engine)?;
        let network = catalog
            .network(&name)
            .ok_or_else(|| RpcStatus::not_found(format!("network {name}")))?;
        Ok(Response::new(network_to_proto(network)))
    }

    async fn list_networks(
        &self,
        request: Request<ListNetworksRequest>,
    ) -> Result<Response<ListNetworksResponse>, RpcStatus> {
        self.auth(&request)?;
        let engine = self.lock()?;
        let catalog = load(&engine)?;
        Ok(Response::new(ListNetworksResponse {
            networks: catalog.networks.iter().map(network_to_proto).collect(),
        }))
    }

    async fn delete_network(
        &self,
        request: Request<DeleteNetworkRequest>,
    ) -> Result<Response<DeleteNetworkResponse>, RpcStatus> {
        self.auth(&request)?;
        let name = request.into_inner().name;
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        let before = catalog.networks.len();
        catalog.networks.retain(|n| n.name != name);
        catalog.reservations.retain(|r| r.network != name);
        let deleted = catalog.networks.len() != before;
        if deleted && catalog.networks.is_empty() {
            return Err(RpcStatus::failed_precondition(
                "the last network cannot be deleted",
            ));
        }
        if deleted {
            save(&mut engine, &catalog)?;
        }
        Ok(Response::new(DeleteNetworkResponse { deleted }))
    }

    async fn put_reservation(
        &self,
        request: Request<ProtoReservation>,
    ) -> Result<Response<ProtoReservation>, RpcStatus> {
        self.auth(&request)?;
        let reservation = reservation_from_proto(&request.into_inner())?;
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        catalog
            .reservations
            .retain(|r| r.mac_address != reservation.mac_address);
        catalog.reservations.push(reservation.clone());
        save(&mut engine, &catalog)?;
        Ok(Response::new(reservation_to_proto(&reservation)))
    }

    async fn list_reservations(
        &self,
        request: Request<ListReservationsRequest>,
    ) -> Result<Response<ListReservationsResponse>, RpcStatus> {
        self.auth(&request)?;
        let network = request.into_inner().network;
        let engine = self.lock()?;
        let catalog = load(&engine)?;
        let reservations = catalog
            .reservations
            .iter()
            .filter(|r| network.is_empty() || r.network == network)
            .map(reservation_to_proto)
            .collect();
        Ok(Response::new(ListReservationsResponse { reservations }))
    }

    async fn delete_reservation(
        &self,
        request: Request<DeleteReservationRequest>,
    ) -> Result<Response<DeleteReservationResponse>, RpcStatus> {
        self.auth(&request)?;
        let req = request.into_inner();
        let mac =
            crate::model::normalize_mac(&req.mac_address).map_err(RpcStatus::invalid_argument)?;
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        let before = catalog.reservations.len();
        catalog
            .reservations
            .retain(|r| !(r.network == req.network && r.mac_address.eq_ignore_ascii_case(&mac)));
        let deleted = catalog.reservations.len() != before;
        if deleted {
            save(&mut engine, &catalog)?;
        }
        Ok(Response::new(DeleteReservationResponse { deleted }))
    }

    async fn put_record(
        &self,
        request: Request<ProtoRecord>,
    ) -> Result<Response<ProtoRecord>, RpcStatus> {
        self.auth(&request)?;
        let record = record_from_proto(&request.into_inner())?;
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        catalog.records.retain(|r| {
            !(r.name == record.name && r.rrtype == record.rrtype && r.content == record.content)
        });
        catalog.records.push(record.clone());
        save(&mut engine, &catalog)?;
        Ok(Response::new(record_to_proto(&record)))
    }

    async fn list_records(
        &self,
        request: Request<ListRecordsRequest>,
    ) -> Result<Response<ListRecordsResponse>, RpcStatus> {
        self.auth(&request)?;
        let zone = request.into_inner().zone;
        let engine = self.lock()?;
        let catalog = load(&engine)?;
        let records = catalog
            .records
            .iter()
            .filter(|r| zone.is_empty() || r.zone == zone)
            .map(record_to_proto)
            .collect();
        Ok(Response::new(ListRecordsResponse { records }))
    }

    async fn delete_record(
        &self,
        request: Request<DeleteRecordRequest>,
    ) -> Result<Response<DeleteRecordResponse>, RpcStatus> {
        self.auth(&request)?;
        let req = request.into_inner();
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        let before = catalog.records.len();
        catalog.records.retain(|r| {
            !(r.zone == req.zone
                && r.name == req.name
                && r.rrtype == req.rrtype
                && r.content == req.content)
        });
        let deleted = catalog.records.len() != before;
        if deleted {
            save(&mut engine, &catalog)?;
        }
        Ok(Response::new(DeleteRecordResponse { deleted }))
    }

    async fn import_router_os(
        &self,
        request: Request<ImportRouterOsRequest>,
    ) -> Result<Response<ImportRouterOsResponse>, RpcStatus> {
        self.auth(&request)?;
        let req = request.into_inner();
        let mut engine = self.lock()?;
        let mut catalog = load(&engine)?;
        let report = routeros::merge_routeros(&mut catalog, &req.document, &req.network)
            .map_err(|e| RpcStatus::invalid_argument(e.to_string()))?;
        save(&mut engine, &catalog)?;
        Ok(Response::new(ImportRouterOsResponse {
            reservations: report.reservations as u32,
            skipped: report.skipped as u32,
            warnings: report.warnings,
            networks: report.networks,
        }))
    }

    async fn apply(
        &self,
        request: Request<ApplyRequest>,
    ) -> Result<Response<ApplyResponse>, RpcStatus> {
        self.auth(&request)?;
        let mut engine = self.lock()?;
        let response = engine
            .apply()
            .map_err(|e| RpcStatus::internal(e.to_string()))?;
        Ok(Response::new(response))
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<Status>, RpcStatus> {
        self.auth(&request)?;
        let engine = self.lock()?;
        let catalog = load(&engine)?;
        let zones = crate::render::zone_plan(&catalog)
            .into_iter()
            .map(|z| z.name)
            .collect();
        Ok(Response::new(Status {
            reservation_count: catalog.reservations.len() as u32,
            upstream: catalog.upstream.clone(),
            authoritative: catalog.authoritative.clone(),
            networks: catalog.networks.iter().map(network_to_proto).collect(),
            zones,
        }))
    }
}

fn load(engine: &Engine) -> Result<Catalog, RpcStatus> {
    engine
        .store
        .load()
        .map_err(|e| RpcStatus::internal(e.to_string()))
}

fn save(engine: &mut Engine, catalog: &Catalog) -> Result<(), RpcStatus> {
    catalog.validate().map_err(RpcStatus::invalid_argument)?;
    engine
        .store
        .save(catalog)
        .map_err(|e| RpcStatus::internal(e.to_string()))
}

fn network_from_proto(proto: &ProtoNetwork) -> Result<Network, RpcStatus> {
    Ok(Network {
        name: proto.name.clone(),
        interface: proto.interface.clone(),
        cidr: proto.cidr.clone(),
        gateway: proto.gateway.clone(),
        domain: proto.domain.clone(),
        dns_servers: proto.dns_servers.clone(),
        pools: proto
            .pools
            .iter()
            .map(|p| AddressPool {
                start: p.start.clone(),
                end: p.end.clone(),
            })
            .collect(),
        dhcp_enabled: proto.dhcp_enabled,
        valid_lifetime: if proto.valid_lifetime == 0 {
            1800
        } else {
            proto.valid_lifetime
        },
        next_server: proto.next_server.clone(),
        boot_file_name: proto.boot_file_name.clone(),
    })
}

fn network_to_proto(network: &Network) -> ProtoNetwork {
    ProtoNetwork {
        name: network.name.clone(),
        interface: network.interface.clone(),
        cidr: network.cidr.clone(),
        gateway: network.gateway.clone(),
        domain: network.domain.clone(),
        dns_servers: network.dns_servers.clone(),
        pools: network
            .pools
            .iter()
            .map(|p| crate::proto::AddressPool {
                start: p.start.clone(),
                end: p.end.clone(),
            })
            .collect(),
        dhcp_enabled: network.dhcp_enabled,
        valid_lifetime: network.valid_lifetime,
        next_server: network.next_server.clone(),
        boot_file_name: network.boot_file_name.clone(),
    }
}

fn reservation_from_proto(proto: &ProtoReservation) -> Result<Reservation, RpcStatus> {
    let mac =
        crate::model::normalize_mac(&proto.mac_address).map_err(RpcStatus::invalid_argument)?;
    Ok(Reservation {
        network: proto.network.clone(),
        mac_address: mac,
        ip_address: proto.ip_address.clone(),
        hostname: proto.hostname.clone(),
        comment: proto.comment.clone(),
        static_lease: proto.static_lease,
    })
}

fn reservation_to_proto(reservation: &Reservation) -> ProtoReservation {
    ProtoReservation {
        network: reservation.network.clone(),
        mac_address: reservation.mac_address.clone(),
        ip_address: reservation.ip_address.clone(),
        hostname: reservation.hostname.clone(),
        comment: reservation.comment.clone(),
        static_lease: reservation.static_lease,
    }
}

fn record_from_proto(proto: &ProtoRecord) -> Result<DnsRecord, RpcStatus> {
    if proto.name.is_empty() || proto.rrtype.is_empty() || proto.content.is_empty() {
        return Err(RpcStatus::invalid_argument(
            "record needs a name, type, and content",
        ));
    }
    Ok(DnsRecord {
        zone: proto.zone.clone(),
        name: proto.name.clone(),
        rrtype: proto.rrtype.clone(),
        content: proto.content.clone(),
        ttl: if proto.ttl == 0 { 3600 } else { proto.ttl },
    })
}

fn record_to_proto(record: &DnsRecord) -> ProtoRecord {
    ProtoRecord {
        zone: record.zone.clone(),
        name: record.name.clone(),
        rrtype: record.rrtype.clone(),
        content: record.content.clone(),
        ttl: record.ttl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_catalog;
    use crate::proto::network_provisioner_client::NetworkProvisionerClient;
    use crate::proto::{AddressPool as ProtoPool, ListNetworksRequest};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Channel;

    #[tokio::test]
    async fn grpc_roundtrip_lists_two_networks() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let mut engine = Engine {
            store: Store::open(&layout.database()).unwrap(),
            layout,
            token: String::new(),
            sync_dns: false,
            reload: false,
        };
        engine.store.save(&sample_catalog()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = NetworkProvisionerServer::new(Service::new(engine));
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let channel = {
            let mut connected = None;
            for _ in 0..50 {
                match Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                {
                    Ok(channel) => {
                        connected = Some(channel);
                        break;
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
            connected.expect("grpc server")
        };
        let mut client = NetworkProvisionerClient::new(channel);
        let listed = client
            .list_networks(ListNetworksRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(listed.networks.len(), 2);
        let applied = client.apply(ApplyRequest {}).await.unwrap().into_inner();
        assert!(applied.zones.iter().any(|z| z == "garage.home.arpa."));
        let status = client
            .get_status(GetStatusRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.reservation_count, 1);
        assert_eq!(status.upstream, vec!["1.1.1.1", "8.8.8.8"]);
        let _ = ProtoPool {
            start: String::new(),
            end: String::new(),
        };
    }
}
