//! Pure renderers for Slurm host services.
//!
//! `slurm.conf` is identical on every member. Only the per-node fragment
//! (`/etc/nixos/kcore-slurm.nix`) differs by `roles`.

use crate::slurm_cluster_spec::{Member, SlurmClusterSpec};
use kcore_sanitize::nix_escape;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEntry {
    pub model: String,
    pub address: String,
}

pub fn render_slurm_conf(
    spec: &SlurmClusterSpec,
    members: &BTreeMap<String, Member>,
    gpus: &BTreeMap<String, Vec<GpuEntry>>,
) -> String {
    let mut out = String::new();
    out.push_str("SelectType=select/cons_tres\n");
    out.push_str("SelectTypeParameters=CR_Core_Memory\n");
    out.push_str("ProctrackType=proctrack/cgroup\n");
    out.push_str("TaskPlugin=task/cgroup,task/affinity\n");
    out.push_str("SchedulerType=sched/backfill\n");
    out.push_str("SrunPortRange=60001-63000\n");
    out.push_str("ReturnToService=2\n");
    out.push_str("SlurmdTimeout=300\n");
    if let Some(backup) = &spec.backup_controller {
        out.push_str(&format!(
            "BackupController={}\nBackupAddr={}\n",
            line(&backup.node_id),
            line(&backup.address)
        ));
    }
    if spec.accounting.enabled {
        let dbd = accounting_node_id(spec);
        let address = members.get(&dbd).map(|m| m.address.as_str()).unwrap_or("");
        out.push_str("AccountingStorageType=accounting_storage/slurmdbd\n");
        out.push_str("JobAcctGatherType=jobacct_gather/cgroup\n");
        out.push_str("JobAcctGatherFrequency=30\n");
        if !address.is_empty() {
            out.push_str(&format!(
                "AccountingStorageHost={}\nAccountingStoragePort=6819\n",
                line(address)
            ));
        }
    }

    for member in members.values() {
        let Some(compute) = &member.compute else {
            continue;
        };
        let mut line_out = format!(
            "NodeName={} NodeAddr={}",
            line(&compute.node_id),
            line(&compute.address)
        );
        if compute.reserve.cores > 0 {
            line_out.push_str(&format!(" CoreSpecCount={}", compute.reserve.cores));
        }
        if compute.reserve.memory_mb > 0 {
            line_out.push_str(&format!(" MemSpecLimit={}", compute.reserve.memory_mb));
        }
        if compute.gres == "auto" {
            let count = gpus.get(&compute.node_id).map(Vec::len).unwrap_or(0);
            if count > 0 {
                line_out.push_str(&format!(" Gres=gpu:{count}"));
            }
        }
        line_out.push_str(" State=UNKNOWN\n");
        out.push_str(&line_out);
    }

    for partition in &spec.partitions {
        let nodes: Vec<String> = members
            .values()
            .filter(|m| {
                m.compute
                    .as_ref()
                    .is_some_and(|c| c.partitions.iter().any(|p| p == &partition.name))
            })
            .map(|m| m.node_id.clone())
            .collect();
        if nodes.is_empty() {
            continue;
        }
        let state = if partition.state.trim().is_empty() {
            "UP"
        } else {
            partition.state.as_str()
        };
        let max_time = if partition.max_time.trim().is_empty() {
            "INFINITE"
        } else {
            partition.max_time.as_str()
        };
        out.push_str(&format!(
            "PartitionName={} Nodes={} Default={} MaxTime={} State={}\n",
            line(&partition.name),
            nodes.iter().map(|n| line(n)).collect::<Vec<_>>().join(","),
            if partition.default { "YES" } else { "NO" },
            line(max_time),
            line(state)
        ));
    }

    if !spec.extra_config.trim().is_empty() {
        out.push_str(spec.extra_config.trim_end());
        out.push('\n');
    }
    out
}

pub fn render_cgroup_conf() -> String {
    [
        "CgroupPlugin=autodetect",
        "ConstrainCores=yes",
        "ConstrainDevices=yes",
        "ConstrainRAMSpace=yes",
        "IgnoreSystemd=no",
    ]
    .join("\n")
        + "\n"
}

pub fn render_gres_conf(spec: &SlurmClusterSpec, gpus: &BTreeMap<String, Vec<GpuEntry>>) -> String {
    let mut out = String::new();
    for compute in &spec.compute {
        if compute.gres != "auto" {
            continue;
        }
        let entries = gpus.get(&compute.node_id).cloned().unwrap_or_default();
        let mut by_model: BTreeMap<String, usize> = BTreeMap::new();
        for entry in entries {
            *by_model.entry(entry.model).or_insert(0) += 1;
        }
        for (model, count) in by_model {
            let model = if model.trim().is_empty() {
                "gpu"
            } else {
                model.as_str()
            };
            out.push_str(&format!(
                "NodeName={} Name=gpu Type={} Count={}\n",
                line(&compute.node_id),
                line(model),
                count
            ));
        }
    }
    out
}

pub fn render_node_fragment(
    spec: &SlurmClusterSpec,
    member: &Member,
    members: &BTreeMap<String, Member>,
    gpus: &BTreeMap<String, Vec<GpuEntry>>,
    gpu_driver: bool,
) -> String {
    let slurm_conf = render_slurm_conf(spec, members, gpus);
    let cgroup_conf = render_cgroup_conf();
    let gres_conf = render_gres_conf(spec, gpus);
    let control = &spec.controller;
    let (backup_machine, backup_addr) = spec
        .backup_controller
        .as_ref()
        .map(|b| (b.node_id.as_str(), b.address.as_str()))
        .unwrap_or(("", ""));
    let gpu_ids: Vec<String> = gpus
        .get(&member.node_id)
        .map(|entries| entries.iter().map(|e| e.address.clone()).collect())
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("{ ... }:\n{\n  kcore.slurm = {\n");
    out.push_str("    enable = true;\n");
    out.push_str(&format!(
        "    nodeId = {};\n    nodeAddress = {};\n",
        nix_string(&member.node_id),
        nix_string(&member.address)
    ));
    out.push_str(&format!(
        "    clusterName = {};\n",
        nix_string(&spec.cluster_name)
    ));
    out.push_str(&format!(
        "    roles = [ {} ];\n",
        member
            .roles
            .iter()
            .map(|r| nix_string(r))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    out.push_str(&format!(
        "    controlMachine = {};\n    controlAddr = {};\n",
        nix_string(&control.node_id),
        nix_string(&control.address)
    ));
    out.push_str(&format!(
        "    backupControlMachine = {};\n    backupControlAddr = {};\n",
        nix_string(backup_machine),
        nix_string(backup_addr)
    ));
    out.push_str(&format!("    iface = {};\n", nix_string(&member.iface)));
    out.push_str("    memberAddresses = [\n");
    for other in members.values() {
        out.push_str(&format!("      {}\n", nix_string(&other.address)));
    }
    out.push_str("    ];\n    memberHosts = [\n");
    for other in members.values() {
        out.push_str(&format!(
            "      {{ name = {}; address = {}; }}\n",
            nix_string(&other.node_id),
            nix_string(&other.address)
        ));
    }
    out.push_str("    ];\n");
    out.push_str(&format!("    slurmConf = {};\n", nix_string(&slurm_conf)));
    out.push_str(&format!("    cgroupConf = {};\n", nix_string(&cgroup_conf)));
    out.push_str(&format!("    gresConf = {};\n", nix_string(&gres_conf)));
    out.push_str("    users = [\n");
    for user in &spec.users {
        out.push_str(&format!(
            "      {{ name = {}; uid = {}; sshKeys = [ {} ]; }}\n",
            nix_string(&user.name),
            user.uid,
            user.ssh_keys
                .iter()
                .map(|k| nix_string(k))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    out.push_str("    ];\n");
    out.push_str("    storage = {\n");
    out.push_str(&format!(
        "      mode = {};\n      exports = [ {} ];\n      filesystem = {};\n",
        nix_string(&spec.storage.mode),
        spec.storage
            .exports
            .iter()
            .map(|e| nix_string(e))
            .collect::<Vec<_>>()
            .join(" "),
        nix_string(&spec.storage.filesystem)
    ));
    out.push_str("    };\n");
    out.push_str(&format!(
        "    gpu = {{ driver = {}; pciIds = [ {} ]; }};\n",
        gpu_driver,
        gpu_ids
            .iter()
            .map(|id| nix_string(id))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    out.push_str(&format!(
        "    accounting = {{ enabled = {}; nodeId = {}; }};\n",
        spec.accounting.enabled,
        nix_string(&accounting_node_id(spec))
    ));
    out.push_str("  };\n}\n");
    out
}

pub fn config_hash(fragment: &str) -> String {
    // FNV-1a keeps this dependency-free; the hash is an equality token, not a
    // security primitive.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in fragment.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn accounting_node_id(spec: &SlurmClusterSpec) -> String {
    if spec.accounting.node_id.trim().is_empty() {
        spec.controller.node_id.clone()
    } else {
        spec.accounting.node_id.clone()
    }
}

/// Role order used by the reconciler and cluster updates.
pub fn role_rank(roles: &[String]) -> u8 {
    if roles.iter().any(|r| r == "dbd") {
        0
    } else if roles.iter().any(|r| r == "controller") {
        1
    } else if roles.iter().any(|r| r == "compute") {
        2
    } else {
        3
    }
}

pub fn all_partition_names(spec: &SlurmClusterSpec) -> BTreeSet<String> {
    spec.partitions.iter().map(|p| p.name.clone()).collect()
}

fn line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

fn nix_string(value: &str) -> String {
    let escaped = nix_escape(value).replace('\r', "\\r").replace('\n', "\\n");
    format!("\"{escaped}\"")
}

pub fn render_disabled_fragment() -> String {
    "{ ... }:\n{\n  kcore.slurm = {\n    enable = false;\n  };\n}\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn nix_escaping_leaves_no_raw_interpolation(value in ".*") {
            let escaped = nix_string(&value);
            let without_escapes = escaped.replace("\\${", "");
            prop_assert!(!without_escapes.contains("${"), "raw interpolation in {}", escaped);
            prop_assert!(escaped.starts_with('"'));
            prop_assert!(escaped.ends_with('"'));
        }
    }
    use crate::slurm_cluster_spec::{
        SlurmAccountingSpec, SlurmComputeSpec, SlurmControllerSpec, SlurmLoginSpec,
        SlurmPartitionSpec, SlurmReserveSpec, SlurmStorageSpec, SlurmUserSpec,
    };

    fn fixture() -> (
        SlurmClusterSpec,
        BTreeMap<String, Member>,
        BTreeMap<String, Vec<GpuEntry>>,
    ) {
        let spec = SlurmClusterSpec {
            cluster_name: "hpc-a".into(),
            controller: SlurmControllerSpec {
                node_id: "node-1".into(),
                address: "10.0.0.1".into(),
                iface: "enp6s0".into(),
            },
            backup_controller: None,
            login: SlurmLoginSpec {
                node_ids: vec!["node-1".into()],
            },
            accounting: SlurmAccountingSpec::default(),
            compute: vec![SlurmComputeSpec {
                node_id: "node-2".into(),
                address: "10.0.0.2".into(),
                iface: "enp6s0".into(),
                partitions: vec!["batch".into()],
                reserve: SlurmReserveSpec {
                    cores: 2,
                    memory_mb: 4096,
                },
                gres: "none".into(),
            }],
            partitions: vec![SlurmPartitionSpec {
                name: "batch".into(),
                default: true,
                max_time: "INFINITE".into(),
                state: "UP".into(),
            }],
            users: vec![SlurmUserSpec {
                name: "alice".into(),
                uid: 2001,
                ssh_keys: vec!["ssh-ed25519 AAAA".into()],
            }],
            storage: SlurmStorageSpec::default(),
            extra_config: String::new(),
            allow_controller_host: false,
            gpu_driver: false,
        };
        let members = crate::slurm_cluster_spec::members(&spec, |_| None);
        (spec, members, BTreeMap::new())
    }

    #[test]
    fn slurm_conf_has_scheduling_defaults_and_reserve() {
        let (spec, members, gpus) = fixture();
        let conf = render_slurm_conf(&spec, &members, &gpus);
        assert!(conf.contains("SelectType=select/cons_tres"));
        assert!(conf.contains("SelectTypeParameters=CR_Core_Memory"));
        assert!(conf.contains("ProctrackType=proctrack/cgroup"));
        assert!(conf.contains("TaskPlugin=task/cgroup,task/affinity"));
        assert!(conf.contains("SchedulerType=sched/backfill"));
        assert!(conf.contains("SrunPortRange=60001-63000"));
        assert!(
            conf.contains("NodeName=node-2 NodeAddr=10.0.0.2 CoreSpecCount=2 MemSpecLimit=4096")
        );
        assert!(conf.contains("PartitionName=batch Nodes=node-2 Default=YES"));
        assert!(!conf.contains("AccountingStorageType"));
    }

    #[test]
    fn cgroup_conf_constrains_cpu_memory_and_devices() {
        let conf = render_cgroup_conf();
        assert!(conf.contains("ConstrainCores=yes"));
        assert!(conf.contains("ConstrainRAMSpace=yes"));
        assert!(conf.contains("ConstrainDevices=yes"));
    }

    #[test]
    fn gres_conf_uses_inventory() {
        let (mut spec, _members, _gpus) = fixture();
        spec.compute[0].gres = "auto".into();
        let mut gpus = BTreeMap::new();
        gpus.insert(
            "node-2".to_string(),
            vec![
                GpuEntry {
                    model: "NVIDIA A100".into(),
                    address: "0000:41:00.0".into(),
                },
                GpuEntry {
                    model: "NVIDIA A100".into(),
                    address: "0000:42:00.0".into(),
                },
            ],
        );
        let conf = render_gres_conf(&spec, &gpus);
        assert_eq!(conf, "NodeName=node-2 Name=gpu Type=NVIDIA A100 Count=2\n");
    }

    #[test]
    fn matches_golden_files() {
        let (mut spec, members, mut gpus) = fixture();
        let conf = render_slurm_conf(&spec, &members, &gpus);
        assert_eq!(
            conf,
            include_str!("../../../tests/fixtures/slurm/slurm.conf")
        );
        assert_eq!(
            render_cgroup_conf(),
            include_str!("../../../tests/fixtures/slurm/cgroup.conf")
        );
        spec.compute[0].gres = "auto".into();
        gpus.insert(
            "node-2".to_string(),
            vec![
                GpuEntry {
                    model: "NVIDIA A100".into(),
                    address: "0000:41:00.0".into(),
                },
                GpuEntry {
                    model: "NVIDIA A100".into(),
                    address: "0000:42:00.0".into(),
                },
            ],
        );
        assert_eq!(
            render_gres_conf(&spec, &gpus),
            include_str!("../../../tests/fixtures/slurm/gres.conf")
        );
    }

    #[test]
    fn node_fragment_escapes_interpolation() {
        let (mut spec, members, gpus) = fixture();
        spec.extra_config = "Bad ${x} \"quote\" \\ slash\n".into();
        let fragment = render_node_fragment(&spec, &members["node-1"], &members, &gpus, false);
        assert!(fragment.contains("\\${x}"));
        assert!(fragment.contains("\\\"quote\\\""));
        assert!(fragment.contains("\\\\ slash"));
        assert!(fragment.contains("roles = [ \"controller\" \"login\" ]"));
        assert!(fragment.contains("enable = true"));
    }
}
