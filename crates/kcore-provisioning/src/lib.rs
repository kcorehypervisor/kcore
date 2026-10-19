//! Multi-network DHCP and DNS provisioner.
//!
//! Kea serves one subnet per network. PowerDNS authoritative holds each
//! network's forward and reverse zone. The recursor forwards those zones
//! locally and sends everything else to the configured upstream resolvers.

pub mod apply;
pub mod grpc;
pub mod model;
pub mod pdns;
pub mod render;
pub mod routeros;
pub mod store;

pub mod proto {
    tonic::include_proto!("kcore.network.v1");
}
