//! Render `/etc/nixos/kcore-postgresql-cluster.nix` for one member.
//!
//! The values file only sets `kcore.postgresqlCluster` options; the module
//! (`modules/kcore-postgresql-cluster.nix`) is imported by the host module.
//! Every string goes through `nix_escape`. Nothing secret is rendered.

use kcore_sanitize::nix_escape;
use serde_json::Value;

use crate::ops;
use crate::spec::{self, ClusterSpec};

pub const VALUES_FILE: &str = "/etc/nixos/kcore-postgresql-cluster.nix";
pub const ROOT: &str = "/var/lib/kcore-postgresql";

pub fn cluster_root(cluster: &str) -> String {
    format!("{ROOT}/{cluster}")
}

pub fn secrets_dir(cluster: &str) -> String {
    format!("{ROOT}/{cluster}/secrets")
}

pub fn tls_dir(cluster: &str) -> String {
    format!("{ROOT}/{cluster}/tls")
}

/// Mount point of the data volume; the data directory is `<major>` below it,
/// so `pg_upgrade --clone` stays on one filesystem.
pub fn data_mount(cluster: &str) -> String {
    format!("{ROOT}/{cluster}/pgdata")
}

pub fn data_path(cluster: &str, major: u32) -> String {
    format!("{ROOT}/{cluster}/pgdata/{major}")
}

pub fn wal_mount(cluster: &str) -> String {
    format!("{ROOT}/{cluster}/pgwal")
}

pub fn wal_path(cluster: &str, major: u32) -> String {
    format!("{ROOT}/{cluster}/pgwal/{major}")
}

pub fn etcd_mount(cluster: &str) -> String {
    format!("{ROOT}/{cluster}/etcd")
}

pub fn etcd_unit(cluster: &str) -> String {
    format!("kcore-pg-etcd-{cluster}.service")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtcdPeer {
    pub name: String,
    pub host: String,
}

/// Everything one member's values file needs.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberFragment {
    pub cluster: String,
    pub generation: i64,
    pub package: String,
    pub node_id: String,
    pub advertise_address: String,
    pub iface: String,
    pub tls: bool,
    pub ports: spec::Ports,
    pub etcd_voter: bool,
    pub etcd_peers: Vec<EtcdPeer>,
    /// "new" or "existing"
    pub etcd_initial_state: String,
    pub postgres: bool,
    pub wal_volume: bool,
    pub members: Vec<String>,
    pub client_networks: Vec<String>,
    pub memory_limit_bytes: u64,
    pub watchdog: String,
    pub no_failover: bool,
    pub no_load_balance: bool,
    pub failover_priority: u32,
    pub extension_packages: Vec<String>,
    pub previous_package: Option<String>,
    pub bootstrap_dcs: Value,
    /// `address/prefix` and router id when this member carries the VIP.
    pub vip: Option<(String, u32, u32)>,
    pub volumes: Vec<Volume>,
}

/// A declared volume of this member, mounted by the values file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub mount_point: String,
    pub device: String,
    pub fs_type: String,
    pub storage_class: String,
    pub pool: String,
    pub name: String,
    pub size_bytes: i64,
}

/// Volumes a member declares, with their mount points.
pub fn member_volumes(cluster: &str, member: &spec::Member) -> Vec<Volume> {
    let mut out = Vec::new();
    for (mount, volume) in [
        (data_mount(cluster), &member.data_volume),
        (wal_mount(cluster), &member.wal_volume),
        (etcd_mount(cluster), &member.etcd_volume),
    ] {
        let Some(v) = volume else { continue };
        let device = if v.storage_class == "zfs" {
            format!("/dev/zvol/{}/{}", v.volume_group, v.name)
        } else {
            format!("/dev/{}/{}", v.volume_group, v.name)
        };
        out.push(Volume {
            mount_point: mount,
            device,
            fs_type: if v.fs_type.is_empty() {
                "xfs".into()
            } else {
                v.fs_type.clone()
            },
            storage_class: if v.storage_class.is_empty() {
                "lvm".into()
            } else {
                v.storage_class.clone()
            },
            pool: v.volume_group.clone(),
            name: v.name.clone(),
            size_bytes: v.size_bytes,
        });
    }
    out
}

/// Which parts of the member are switched on. Create applies `EtcdOnly` on
/// every voter first so etcd has quorum before any Patroni starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    EtcdOnly,
    Full,
}

impl MemberFragment {
    /// Build the fragment for `node_id` from the cluster spec. `voters` are
    /// the etcd voter node ids (all initial members; members added by scale
    /// are not voters).
    pub fn from_spec(
        cluster: &str,
        generation: i64,
        spec: &ClusterSpec,
        node_id: &str,
        voters: &[String],
        etcd_initial_state: &str,
        previous_package: Option<&str>,
    ) -> Option<Self> {
        let member = spec.member(node_id)?;
        let etcd_peers = spec
            .members
            .iter()
            .filter(|m| voters.iter().any(|v| v == &m.node_id))
            .map(|m| EtcdPeer {
                name: m.node_id.clone(),
                host: m.advertise_address.clone(),
            })
            .collect();
        let vip = spec.vip.as_ref().filter(|_| !member.witness).and_then(|v| {
            let (addr, prefix) = spec::parse_cidr(&v.address)?;
            Some((addr.to_string(), u32::from(prefix), v.virtual_router_id))
        });
        Some(Self {
            cluster: cluster.into(),
            generation,
            package: spec.package.clone(),
            node_id: node_id.into(),
            advertise_address: member.advertise_address.clone(),
            iface: member.iface.clone(),
            tls: !spec.tls.disabled,
            ports: spec.ports,
            etcd_voter: voters.iter().any(|v| v == node_id),
            etcd_peers,
            etcd_initial_state: etcd_initial_state.into(),
            postgres: !member.witness,
            wal_volume: member.wal_volume.is_some(),
            members: spec
                .postgres_members()
                .map(|m| m.advertise_address.clone())
                .collect(),
            client_networks: spec.client_networks.clone(),
            memory_limit_bytes: spec.memory_limit_bytes,
            watchdog: spec.patroni.watchdog.clone(),
            no_failover: member.no_failover,
            no_load_balance: member.no_load_balance,
            failover_priority: member.failover_priority.max(1),
            extension_packages: spec::extension_packages(spec),
            previous_package: previous_package.map(str::to_string),
            bootstrap_dcs: ops::dynamic_config(spec),
            vip,
            volumes: member_volumes(cluster, member),
        })
    }
}

fn q(s: &str) -> String {
    format!("\"{}\"", nix_escape(s))
}

fn list(items: &[String], indent: &str) -> String {
    if items.is_empty() {
        return "[ ]".into();
    }
    let mut out = String::from("[\n");
    for item in items {
        out.push_str(&format!("{indent}  {}\n", q(item)));
    }
    out.push_str(&format!("{indent}]"));
    out
}

fn b(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

/// The values file for one member at `stage`.
pub fn render_member(f: &MemberFragment, stage: Stage) -> String {
    let postgres = f.postgres && stage == Stage::Full;
    let mut out = String::new();
    out.push_str("# Written by the kcore node agent. Do not edit: the next apply replaces it.\n");
    out.push_str("_:\n{\n  kcore.postgresqlCluster = {\n");
    out.push_str("    enable = true;\n");
    out.push_str(&format!("    clusterName = {};\n", q(&f.cluster)));
    out.push_str(&format!("    generation = {};\n", f.generation));
    out.push_str(&format!("    package = {};\n", q(&f.package)));
    out.push_str(&format!(
        "    advertiseAddress = {};\n",
        q(&f.advertise_address)
    ));
    out.push_str(&format!("    iface = {};\n", q(&f.iface)));
    out.push_str(&format!("    tls.enable = {};\n", b(f.tls)));
    out.push_str(&format!(
        "    ports = {{\n      postgres = {};\n      patroniApi = {};\n      etcdClient = {};\n      etcdPeer = {};\n    }};\n",
        f.ports.postgres, f.ports.patroni_api, f.ports.etcd_client, f.ports.etcd_peer
    ));
    out.push_str("    etcd = {\n");
    out.push_str(&format!("      enable = {};\n", b(f.etcd_voter)));
    out.push_str(&format!("      name = {};\n", q(&f.node_id)));
    out.push_str(&format!(
        "      initialClusterState = {};\n",
        q(&f.etcd_initial_state)
    ));
    out.push_str("      peers = [\n");
    for peer in &f.etcd_peers {
        out.push_str(&format!(
            "        {{\n          name = {};\n          host = {};\n        }}\n",
            q(&peer.name),
            q(&peer.host)
        ));
    }
    out.push_str("      ];\n    };\n");
    out.push_str("    postgres = {\n");
    out.push_str(&format!("      enable = {};\n", b(postgres)));
    if f.postgres {
        out.push_str(&format!("      walVolume = {};\n", b(f.wal_volume)));
        out.push_str(&format!(
            "      members = {};\n",
            list(&f.members, "      ")
        ));
        out.push_str(&format!(
            "      clientNetworks = {};\n",
            list(&f.client_networks, "      ")
        ));
        out.push_str(&format!(
            "      memoryLimitBytes = {};\n",
            f.memory_limit_bytes
        ));
        out.push_str(&format!("      watchdog = {};\n", q(&f.watchdog)));
        out.push_str(&format!(
            "      tags = {{\n        nofailover = {};\n        noloadbalance = {};\n        failoverPriority = {};\n      }};\n",
            b(f.no_failover),
            b(f.no_load_balance),
            f.failover_priority
        ));
        out.push_str(&format!(
            "      extensionPackages = {};\n",
            list(&f.extension_packages, "      ")
        ));
        if let Some(prev) = &f.previous_package {
            out.push_str(&format!("      previousPackage = {};\n", q(prev)));
        }
        let dcs = serde_json::to_string(&f.bootstrap_dcs).unwrap_or_else(|_| "{}".into());
        out.push_str(&format!(
            "      bootstrapDcs = builtins.fromJSON {};\n",
            q(&dcs)
        ));
    }
    out.push_str("    };\n");
    if let (true, Some((addr, prefix, vrid))) = (postgres, &f.vip) {
        out.push_str(&format!(
            "    vip = {{\n      enable = true;\n      address = {};\n      prefixLength = {prefix};\n      virtualRouterId = {vrid};\n    }};\n",
            q(addr)
        ));
    }
    if !f.volumes.is_empty() {
        out.push_str("    volumes = [\n");
        for v in &f.volumes {
            out.push_str(&format!(
                "      {{\n        mountPoint = {};\n        device = {};\n        fsType = {};\n      }}\n",
                q(&v.mount_point),
                q(&v.device),
                q(&v.fs_type)
            ));
        }
        out.push_str("    ];\n");
    }
    out.push_str("  };\n}\n");
    out
}

/// The values file of a node that is not (or no longer) a member.
pub fn render_disabled() -> String {
    "# Written by the kcore node agent. Do not edit: the next apply replaces it.\n_:\n{\n  kcore.postgresqlCluster.enable = false;\n}\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests::sample;
    use crate::spec::Member;

    fn voters() -> Vec<String> {
        vec!["node-105".into(), "node-106".into(), "node-108".into()]
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/postgresql-cluster")
            .join(name)
    }

    /// `KCORE_UPDATE_GOLDEN=1 cargo test -p kcore-postgresql` rewrites the
    /// fixtures. The Nix eval check imports the same files.
    fn golden(name: &str, rendered: &str) {
        let path = fixture(name);
        if std::env::var_os("KCORE_UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, rendered).unwrap();
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            rendered,
            expected,
            "{} differs; rerun with KCORE_UPDATE_GOLDEN=1",
            path.display()
        );
    }

    fn orders() -> ClusterSpec {
        let (_, mut spec) = sample();
        spec.memory_limit_bytes = 32 << 30;
        spec
    }

    #[test]
    fn member_fragment_matches_golden() {
        let spec = orders();
        let f = MemberFragment::from_spec("orders", 1, &spec, "node-105", &voters(), "new", None)
            .unwrap();
        golden("member.nix", &render_member(&f, Stage::Full));
    }

    #[test]
    fn witness_fragment_matches_golden() {
        let mut spec = orders();
        spec.members.pop();
        spec.members.push(Member {
            node_id: "node-108".into(),
            advertise_address: "192.168.40.108".into(),
            iface: "enp6s0".into(),
            witness: true,
            ..Default::default()
        });
        let f = MemberFragment::from_spec("orders", 1, &spec, "node-108", &voters(), "new", None)
            .unwrap();
        let text = render_member(&f, Stage::Full);
        assert!(text.contains("postgres = {\n      enable = false;\n    };"));
        assert!(!text.contains("vip"));
        golden("witness.nix", &text);
    }

    #[test]
    fn disabled_matches_golden() {
        golden("disabled.nix", &render_disabled());
    }

    #[test]
    fn etcd_only_stage_turns_postgres_and_vip_off() {
        let spec = orders();
        let f = MemberFragment::from_spec("orders", 1, &spec, "node-106", &voters(), "new", None)
            .unwrap();
        let text = render_member(&f, Stage::EtcdOnly);
        assert!(text.contains("etcd = {\n      enable = true;"));
        assert!(text.contains("postgres = {\n      enable = false;"));
        assert!(!text.contains("vip = {"));
    }

    #[test]
    fn non_voter_lists_voters_but_runs_no_etcd() {
        let mut spec = orders();
        spec.members.push(Member {
            node_id: "node-110".into(),
            advertise_address: "192.168.40.110".into(),
            iface: "enp6s0".into(),
            ..Default::default()
        });
        let f = MemberFragment::from_spec("orders", 2, &spec, "node-110", &voters(), "new", None)
            .unwrap();
        assert!(!f.etcd_voter);
        assert_eq!(f.etcd_peers.len(), 3);
        assert_eq!(f.members.len(), 4);
    }

    #[test]
    fn strings_are_escaped() {
        let spec = orders();
        let mut f =
            MemberFragment::from_spec("orders", 1, &spec, "node-105", &voters(), "new", None)
                .unwrap();
        f.iface = "en\"${builtins.abort \"x\"}".into();
        let text = render_member(&f, Stage::Full);
        assert!(text.contains("iface = \"en\\\"\\${builtins.abort \\\"x\\\"}\";"));
    }

    #[test]
    fn previous_package_is_rendered_during_an_upgrade() {
        let mut spec = orders();
        spec.package = "postgresql_18".into();
        let f = MemberFragment::from_spec(
            "orders",
            3,
            &spec,
            "node-105",
            &voters(),
            "new",
            Some("postgresql_17"),
        )
        .unwrap();
        let text = render_member(&f, Stage::Full);
        assert!(text.contains("package = \"postgresql_18\";"));
        assert!(text.contains("previousPackage = \"postgresql_17\";"));
    }

    proptest::proptest! {
        #[test]
        fn any_iface_string_stays_inside_its_literal(s in "\\PC{0,40}") {
            let spec = orders();
            let mut f = MemberFragment::from_spec("orders", 1, &spec, "node-105", &voters(), "new", None).unwrap();
            f.iface = s;
            let text = render_member(&f, Stage::Full);
            let line = text.lines().find(|l| l.trim_start().starts_with("iface = ")).unwrap();
            let body = line.trim().trim_start_matches("iface = ").trim_end_matches(';');
            let inner = &body[1..body.len() - 1];
            let mut chars = inner.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' { chars.next(); continue; }
                proptest::prop_assert!(c != '"');
                let opens_interpolation = c == '$' && chars.peek() == Some(&'{');
                proptest::prop_assert!(!opens_interpolation);
            }
        }
    }
}
