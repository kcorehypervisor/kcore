use crate::config::NetworkConfig;
use crate::db::{NetworkRow, VmNicRow, VmRow};

#[derive(Debug, Clone)]
pub struct VxlanMeta {
    pub vni: i32,
    pub peers: Vec<String>,
    pub local_ip: String,
}

#[derive(Debug, Clone)]
pub struct SecurityGroupResolvedRule {
    pub protocol: String,
    pub host_port: i32,
    pub target_port: i32,
    pub source_cidr: String,
    pub target_ip: String,
    pub enable_dnat: bool,
}

fn netmask_to_cidr(mask: &str) -> u8 {
    mask.split('.')
        .filter_map(|o| o.parse::<u8>().ok())
        .map(|o| o.count_ones() as u8)
        .sum()
}

// `nix_escape` and `sanitize_nix_attr_key` live in the leaf
// `kcore-sanitize` crate, where they are exhaustively verified
// with Kani. Re-exported here so the rest of `nixgen.rs` keeps
// using the unqualified names.
use kcore_sanitize::{nix_escape, sanitize_nix_attr_key};

/// Which newer per-VM options the destination node's installed ch-vm module
/// understands (see `/etc/kcore/ch-vm-features`). Emitting an option an older
/// module lacks fails that node's whole evaluation, so each one is gated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleFeatures {
    pub image_sha256: bool,
    pub vgpu_profile: bool,
    pub cloud_init_guest_ops_keys: bool,
}

impl ModuleFeatures {
    pub const IMAGE_SHA256: &'static str = "imageSha256";
    pub const VGPU_PROFILE: &'static str = "vgpuProfile";
    pub const CLOUD_INIT_GUEST_OPS_KEYS: &'static str = "cloudInitGuestOpsKeys";

    /// Everything on: what a node installed from this tree supports.
    pub fn all() -> Self {
        Self {
            image_sha256: true,
            vgpu_profile: true,
            cloud_init_guest_ops_keys: true,
        }
    }

    /// From the list the node agent reports. An agent without the RPC, or a
    /// module without the file, yields the baseline (nothing on).
    pub fn from_reported<S: AsRef<str>>(features: &[S]) -> Self {
        let has = |name: &str| features.iter().any(|f| f.as_ref() == name);
        Self {
            image_sha256: has(Self::IMAGE_SHA256),
            vgpu_profile: has(Self::VGPU_PROFILE),
            cloud_init_guest_ops_keys: has(Self::CLOUD_INIT_GUEST_OPS_KEYS),
        }
    }
}

/// One vGPU slice placed on this node. The address is the virtual function.
#[derive(Debug, Clone)]
pub struct VgpuSliceNix {
    pub vm_id: String,
    pub address: String,
    pub profile: String,
    pub host_address: String,
}

pub fn generate_node_config(
    vms: &[VmRow],
    gateway_interface: &str,
    network: &NetworkConfig,
    networks: &[NetworkRow],
    vm_ssh_keys: &std::collections::HashMap<String, Vec<String>>,
    vxlan_peers: &std::collections::HashMap<String, VxlanMeta>,
) -> String {
    generate_node_config_with_security_groups(
        vms,
        gateway_interface,
        network,
        networks,
        vm_ssh_keys,
        vxlan_peers,
        &std::collections::HashMap::new(),
    )
}

pub fn generate_node_config_with_security_groups(
    vms: &[VmRow],
    gateway_interface: &str,
    network: &NetworkConfig,
    networks: &[NetworkRow],
    vm_ssh_keys: &std::collections::HashMap<String, Vec<String>>,
    vxlan_peers: &std::collections::HashMap<String, VxlanMeta>,
    security_group_rules: &std::collections::HashMap<String, Vec<SecurityGroupResolvedRule>>,
) -> String {
    generate_node_config_inner(
        vms,
        gateway_interface,
        network,
        networks,
        vm_ssh_keys,
        vxlan_peers,
        security_group_rules,
        &std::collections::HashSet::new(),
        &[],
        &[],
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &ModuleFeatures::all(),
        &[],
    )
}

fn ipv6_address<'a>(
    vm_ipv6: &'a std::collections::HashMap<String, Vec<crate::db::VmIpv6Row>>,
    vm_id: &str,
    position: i32,
) -> Option<&'a str> {
    vm_ipv6.get(vm_id).and_then(|rows| {
        rows.iter()
            .find(|row| row.position == position && !row.address.is_empty())
            .map(|row| row.address.as_str())
    })
}

pub fn generate_node_config_inner(
    vms: &[VmRow],
    gateway_interface: &str,
    network: &NetworkConfig,
    networks: &[NetworkRow],
    vm_ssh_keys: &std::collections::HashMap<String, Vec<String>>,
    vxlan_peers: &std::collections::HashMap<String, VxlanMeta>,
    security_group_rules: &std::collections::HashMap<String, Vec<SecurityGroupResolvedRule>>,
    incoming_vm_ids: &std::collections::HashSet<String>,
    volumes: &[crate::db::VolumeRow],
    guest_ops_keys: &[String],
    extra_nics: &std::collections::HashMap<String, Vec<VmNicRow>>,
    network_policies: &std::collections::HashMap<String, crate::db::NetworkPolicyRow>,
    vm_ipv6: &std::collections::HashMap<String, Vec<crate::db::VmIpv6Row>>,
    features: &ModuleFeatures,
    vgpu: &[VgpuSliceNix],
) -> String {
    let mut out = String::from("{ pkgs, ... }: {\n");
    out.push_str("  ch-vm.vms = {\n");
    out.push_str("    enable = true;\n");
    out.push_str("    cloudHypervisorPackage = pkgs.cloud-hypervisor;\n");
    out.push_str(&format!(
        "    gatewayInterface = \"{}\";\n",
        nix_escape(gateway_interface)
    ));

    if vms.iter().any(|v| v.storage_backend == "lvm") {
        out.push_str(&format!(
            "    lvmVgName = \"{}\";\n",
            crate::local_migrate::LVM_VG
        ));
    }
    if vms.iter().any(|v| v.storage_backend == "zfs") {
        out.push_str(&format!(
            "    zfsPoolName = \"{}\";\n",
            crate::local_migrate::ZFS_POOL
        ));
    }

    out.push_str("    networks.default = {\n");
    out.push_str(&format!(
        "      externalIP = \"{}\";\n",
        nix_escape(&network.external_ip)
    ));
    out.push_str(&format!(
        "      gatewayIP = \"{}\";\n",
        nix_escape(&network.gateway_ip)
    ));
    if network.internal_netmask != "255.255.255.0" {
        out.push_str(&format!(
            "      internalNetmask = \"{}\";\n",
            nix_escape(&network.internal_netmask)
        ));
    }
    out.push_str("    };\n");

    for net in networks {
        if net.name == "default" {
            continue;
        }
        out.push_str(&format!(
            "    networks.\"{}\" = {{\n",
            nix_escape(&net.name)
        ));
        out.push_str(&format!(
            "      externalIP = \"{}\";\n",
            nix_escape(&net.external_ip)
        ));
        out.push_str(&format!(
            "      gatewayIP = \"{}\";\n",
            nix_escape(&net.gateway_ip)
        ));
        if net.internal_netmask != "255.255.255.0" {
            out.push_str(&format!(
                "      internalNetmask = \"{}\";\n",
                nix_escape(&net.internal_netmask)
            ));
        }
        if !net.allowed_tcp_ports.is_empty() {
            let ports: Vec<&str> = net
                .allowed_tcp_ports
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if !ports.is_empty() {
                out.push_str(&format!(
                    "      allowedTCPPorts = [ {} ];\n",
                    ports.join(" ")
                ));
            }
        }
        if !net.allowed_udp_ports.is_empty() {
            let ports: Vec<&str> = net
                .allowed_udp_ports
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if !ports.is_empty() {
                out.push_str(&format!(
                    "      allowedUDPPorts = [ {} ];\n",
                    ports.join(" ")
                ));
            }
        }
        if net.vlan_id > 0 {
            out.push_str(&format!("      vlanId = {};\n", net.vlan_id));
        }
        if net.network_type != "nat" {
            out.push_str(&format!(
                "      networkType = \"{}\";\n",
                nix_escape(&net.network_type)
            ));
        }
        if !net.enable_outbound_nat {
            out.push_str("      enableOutboundNat = false;\n");
        }
        if let Some(policy) = network_policies.get(&net.name) {
            if policy.east_west {
                out.push_str("      eastWestFirewall = true;\n");
            }
            if !policy.ipv6_prefix.is_empty() {
                out.push_str(&format!(
                    "      ipv6Prefix = \"{}\";\n",
                    nix_escape(&policy.ipv6_prefix)
                ));
                out.push_str(&format!(
                    "      ipv6Gateway = \"{}\";\n",
                    nix_escape(&policy.ipv6_gateway)
                ));
            }
        }
        if let Some(vxlan) = vxlan_peers.get(&net.name) {
            out.push_str(&format!("      vni = {};\n", vxlan.vni));
            let peer_list: Vec<String> = vxlan
                .peers
                .iter()
                .map(|p| format!("\"{}\"", nix_escape(p)))
                .collect();
            out.push_str(&format!(
                "      vxlanPeers = [ {} ];\n",
                peer_list.join(" ")
            ));
            out.push_str(&format!(
                "      vxlanLocalIp = \"{}\";\n",
                nix_escape(&vxlan.local_ip)
            ));
        }
        if let Some(rules) = security_group_rules.get(&net.name) {
            if !rules.is_empty() {
                out.push_str("      securityGroupRules = [\n");
                for rule in rules {
                    out.push_str("        {\n");
                    out.push_str(&format!(
                        "          protocol = \"{}\";\n",
                        nix_escape(&rule.protocol)
                    ));
                    out.push_str(&format!("          hostPort = {};\n", rule.host_port));
                    out.push_str(&format!("          targetPort = {};\n", rule.target_port));
                    out.push_str(&format!(
                        "          sourceCidr = \"{}\";\n",
                        nix_escape(&rule.source_cidr)
                    ));
                    out.push_str(&format!(
                        "          targetIp = \"{}\";\n",
                        nix_escape(&rule.target_ip)
                    ));
                    out.push_str(&format!(
                        "          enableDnat = {};\n",
                        if rule.enable_dnat { "true" } else { "false" }
                    ));
                    out.push_str("        }\n");
                }
                out.push_str("      ];\n");
            }
        }
        out.push_str("    };\n");
    }

    for vm in vms {
        let nix_name = sanitize_nix_attr_key(&vm.name);
        out.push_str(&format!("    virtualMachines.\"{nix_name}\" = {{\n"));
        out.push_str(&format!(
            "      image = \"{}\";\n",
            nix_escape(&vm.image_path)
        ));
        out.push_str(&format!(
            "      imageFormat = \"{}\";\n",
            nix_escape(&vm.image_format)
        ));
        // The provision scripts re-hash the cached image right before seeding.
        // Only a well-formed digest is emitted; anything else would fail the
        // module's strMatching type and take the whole config down with it.
        let sha = vm.image_sha256.trim();
        if features.image_sha256 && sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.push_str(&format!(
                "      imageSha256 = \"{}\";\n",
                sha.to_ascii_lowercase()
            ));
        }
        out.push_str(&format!(
            "      storageBackend = \"{}\";\n",
            nix_escape(&vm.storage_backend)
        ));
        out.push_str(&format!(
            "      storageSizeBytes = {};\n",
            vm.storage_size_bytes
        ));
        if vm.storage_backend == "ceph" {
            let root = volumes.iter().find(|v| {
                v.vm_id == vm.id && v.role == crate::db::VolumeRow::ROLE_ROOT && v.is_attached()
            });
            let rbd_image = root.map(|v| v.image.as_str()).unwrap_or("");
            let rbd_image = if rbd_image.is_empty() {
                format!("kcore-{}", vm.id)
            } else {
                rbd_image.to_string()
            };
            out.push_str(&format!(
                "      rbdImage = \"{}\";\n",
                nix_escape(&rbd_image)
            ));
            let mut data: Vec<_> = volumes
                .iter()
                .filter(|v| {
                    v.vm_id == vm.id && v.role == crate::db::VolumeRow::ROLE_DATA && v.is_attached()
                })
                .collect();
            data.sort_by_key(|v| (v.slot, v.name.as_str()));
            if !data.is_empty() {
                out.push_str("      dataDisks = [\n");
                for v in data {
                    if v.encrypted {
                        let mapper = crate::volume_crypto::mapper_name_for_serial(&v.serial);
                        out.push_str(&format!(
                            "        {{ rbdImage = \"{}\"; serial = \"{}\"; encrypted = true; mapperName = \"{}\"; }}\n",
                            nix_escape(&v.image),
                            nix_escape(&v.serial),
                            nix_escape(&mapper)
                        ));
                    } else {
                        out.push_str(&format!(
                            "        {{ rbdImage = \"{}\"; serial = \"{}\"; }}\n",
                            nix_escape(&v.image),
                            nix_escape(&v.serial)
                        ));
                    }
                }
                out.push_str("      ];\n");
            }
        }
        out.push_str(&format!("      imageSize = {};\n", vm.image_size));
        out.push_str(&format!("      cores = {};\n", vm.cpu));
        out.push_str(&format!(
            "      memorySize = {};\n",
            vm.memory_bytes / (1024 * 1024)
        ));
        out.push_str(&format!(
            "      network = \"{}\";\n",
            nix_escape(&vm.network)
        ));
        out.push_str(&format!(
            "      cloudInitInstanceId = \"{}\";\n",
            nix_escape(&vm.id)
        ));
        if !vm.vm_ip.is_empty() {
            out.push_str(&format!(
                "      dhcpReservedIPv4 = \"{}\";\n",
                nix_escape(&vm.vm_ip)
            ));
        }
        if let Some(addr) = ipv6_address(vm_ipv6, &vm.id, 0) {
            out.push_str(&format!(
                "      dhcpReservedIPv6 = \"{}\";\n",
                nix_escape(addr)
            ));
        }
        let incoming = incoming_vm_ids.contains(&vm.id);
        // Keep autoStart identical across the incoming and settled configs.
        // `incomingMigration` alone keeps systemd from starting the unit. If
        // autoStart flipped too, the final config would change the unit and the
        // switch would restart the unit that just adopted the migrated guest.
        out.push_str(&format!(
            "      autoStart = {};\n",
            if vm.auto_start { "true" } else { "false" }
        ));
        if incoming {
            out.push_str("      incomingMigration = true;\n");
        }
        let pci_devices = crate::pci::split_pci_devices(&vm.pci_devices);
        if !pci_devices.is_empty() {
            out.push_str("      pciDevices = [\n");
            for addr in &pci_devices {
                let profile = vgpu
                    .iter()
                    .find(|slice| slice.vm_id == vm.id && slice.address.eq_ignore_ascii_case(addr));
                if features.vgpu_profile {
                    if let Some(slice) = profile {
                        out.push_str(&format!(
                            "        {{ address = \"{}\"; vgpuProfile = \"{}\"; }}\n",
                            nix_escape(addr),
                            nix_escape(&slice.profile)
                        ));
                        continue;
                    }
                }
                out.push_str(&format!(
                    "        {{ address = \"{}\"; }}\n",
                    nix_escape(addr)
                ));
            }
            out.push_str("      ];\n");
        }
        let mut ssh_keys = vm_ssh_keys.get(&vm.id).cloned().unwrap_or_default();
        for pk in guest_ops_keys {
            if !pk.is_empty() && !ssh_keys.iter().any(|k| k == pk) {
                ssh_keys.push(pk.clone());
            }
        }
        if !vm.cloud_init_user_data.is_empty() {
            let escaped = nix_escape(&vm.cloud_init_user_data);
            out.push_str(&format!(
                "      cloudInitUserConfigFile = pkgs.writeText \"{nix_name}-cloud-init.yaml\" \"{escaped}\";\n"
            ));
            let ops_keys: Vec<&String> = guest_ops_keys.iter().filter(|k| !k.is_empty()).collect();
            if !ops_keys.is_empty() && features.cloud_init_guest_ops_keys {
                out.push_str("      cloudInitGuestOpsKeys = [\n");
                for pk in ops_keys {
                    out.push_str(&format!("        \"{}\"\n", nix_escape(pk)));
                }
                out.push_str("      ];\n");
            }
        } else if !ssh_keys.is_empty() {
            let mut ci = String::from("#cloud-config\n");
            ci.push_str(&format!("hostname: {}\n", nix_name));
            ci.push_str("users:\n");
            ci.push_str("  - default\n");
            ci.push_str("  - name: kcore\n");
            ci.push_str("    gecos: kcore default user\n");
            ci.push_str("    groups: [sudo]\n");
            ci.push_str("    shell: /bin/bash\n");
            ci.push_str("    lock_passwd: true\n");
            ci.push_str("    ssh_authorized_keys:\n");
            for key in &ssh_keys {
                ci.push_str(&format!(
                    "      - \"{}\"\n",
                    key.replace('"', "\\\"").replace('\\', "\\\\")
                ));
            }
            ci.push_str("ssh_pwauth: false\n");
            let escaped = nix_escape(&ci);
            out.push_str(&format!(
                "      cloudInitUserConfigFile = pkgs.writeText \"{nix_name}-cloud-init.yaml\" \"{escaped}\";\n"
            ));
        }

        if !vm.vm_ip.is_empty() {
            let extras = extra_nics.get(&vm.id).map(Vec::as_slice).unwrap_or(&[]);
            if extras.is_empty() {
                let vm_net = networks.iter().find(|n| n.name == vm.network);
                if let Some(net) = vm_net {
                    if net.network_type == "vxlan" {
                        let cidr = netmask_to_cidr(&net.internal_netmask);
                        let mut net_cfg = String::from(
                            "version: 2\nethernets:\n  kcore0:\n    match:\n      name: \"e*\"\n    dhcp4: false\n",
                        );
                        net_cfg.push_str(&format!("    addresses: [\"{}/{}\"", vm.vm_ip, cidr));
                        if let Some(addr) = ipv6_address(vm_ipv6, &vm.id, 0) {
                            net_cfg.push_str(&format!(", \"{addr}/64\""));
                        }
                        net_cfg.push_str("]\n");
                        net_cfg.push_str(&format!("    gateway4: \"{}\"\n", net.gateway_ip));
                        net_cfg.push_str("    nameservers:\n      addresses: [1.1.1.1, 8.8.8.8]\n");
                        let escaped = nix_escape(&net_cfg);
                        out.push_str(&format!(
                            "      cloudInitNetworkConfigFile = pkgs.writeText \"{nix_name}-network-config.yaml\" \"{escaped}\";\n"
                        ));
                    }
                }
            }
        }
        let extras = extra_nics.get(&vm.id).cloned().unwrap_or_default();
        if !extras.is_empty() {
            out.push_str("      extraNics = [\n");
            for nic in &extras {
                out.push_str("        {\n");
                out.push_str(&format!(
                    "          network = \"{}\";\n",
                    nix_escape(&nic.network)
                ));
                if !nic.mac_address.is_empty() {
                    out.push_str(&format!(
                        "          macAddress = \"{}\";\n",
                        nix_escape(&nic.mac_address)
                    ));
                }
                if !nic.ip_address.is_empty() {
                    out.push_str(&format!(
                        "          ipv4 = \"{}\";\n",
                        nix_escape(&nic.ip_address)
                    ));
                }
                if let Some(addr) = ipv6_address(vm_ipv6, &vm.id, nic.position) {
                    out.push_str(&format!("          ipv6 = \"{}\";\n", nix_escape(addr)));
                }
                out.push_str("        }\n");
            }
            out.push_str("      ];\n");
        }

        out.push_str("    };\n");
    }

    out.push_str("  };\n");
    let slice_addrs: std::collections::HashSet<String> = vgpu
        .iter()
        .map(|slice| slice.address.to_ascii_lowercase())
        .collect();
    let needs_vfio = vms.iter().any(|vm| {
        crate::pci::split_pci_devices(&vm.pci_devices)
            .iter()
            .any(|addr| !slice_addrs.contains(&addr.to_ascii_lowercase()))
    });
    if needs_vfio {
        out.push_str("  ch-vm.vfio.enable = true;\n");
    }
    if features.vgpu_profile && !vgpu.is_empty() {
        out.push_str("  ch-vm.vgpu.enable = true;\n");
        let mut hosts: Vec<&str> = vgpu
            .iter()
            .map(|slice| slice.host_address.as_str())
            .collect();
        hosts.sort();
        hosts.dedup();
        out.push_str("  ch-vm.vgpu.hostCards = [\n");
        for host in hosts {
            out.push_str(&format!("    \"{}\"\n", nix_escape(host)));
        }
        out.push_str("  ];\n");
    }
    out.push_str("}\n");
    out
}

/// One database rendered into the node module as `services.postgresql`.
#[derive(Debug, Clone)]
pub struct PostgresqlNix {
    pub database: String,
    pub package: String,
    pub port: i32,
}

/// Append `services.postgresql` to a generated node module.
///
/// An unexpected package or database is left out of the file. Those values
/// are already rejected at the RPC boundary; this is the last check before
/// the text is written as root during `nixos-rebuild`.
pub fn with_postgresql(config: String, pg: Option<&PostgresqlNix>) -> String {
    let Some(pg) = pg else {
        return config;
    };
    let Some(block) = render_postgresql(pg) else {
        return config;
    };
    let trimmed = config.trim_end();
    let Some(body) = trimmed.strip_suffix('}') else {
        return format!("{config}{block}");
    };
    format!("{body}{block}}}\n")
}

fn render_postgresql(pg: &PostgresqlNix) -> Option<String> {
    let package = match pg.package.as_str() {
        "postgresql" | "postgresql_14" | "postgresql_15" | "postgresql_16" | "postgresql_17" => {
            pg.package.as_str()
        }
        _ => return None,
    };
    if !(1..=65535).contains(&pg.port) || !safe_postgresql_ident(&pg.database) {
        return None;
    }
    Some(format!(
        "  services.postgresql = {{\n    enable = true;\n    package = pkgs.{package};\n    port = {};\n    ensureDatabases = [ \"{}\" ];\n  }};\n",
        pg.port,
        nix_escape(&pg.database),
    ))
}

/// The `kcore.ingress` settings of one gateway node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressNix {
    pub listen_addresses: Vec<String>,
    pub http3: bool,
    pub acme_email: String,
    pub vip: Option<IngressVipNix>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressVipNix {
    pub address: String,
    pub interface: String,
    pub virtual_router_id: u8,
    pub priority: u8,
    pub source_address: String,
    pub peers: Vec<String>,
}

/// Append `kcore.ingress` to a generated node module.
///
/// Values come from validated specs, but this text is written as root during
/// `nixos-rebuild`, so addresses must parse as IPs and the interface must be a
/// plain name; anything else leaves the block out.
pub fn with_ingress(config: String, ingress: Option<&IngressNix>) -> String {
    let Some(ingress) = ingress else {
        return config;
    };
    let Some(block) = render_ingress(ingress) else {
        return config;
    };
    splice_module_block(config, &block)
}

fn splice_module_block(config: String, block: &str) -> String {
    let trimmed = config.trim_end();
    let Some(body) = trimmed.strip_suffix('}') else {
        return format!("{config}{block}");
    };
    format!("{body}{block}}}\n")
}

fn render_ingress(ing: &IngressNix) -> Option<String> {
    use std::net::IpAddr;
    let ip_ok = |s: &str| s.parse::<IpAddr>().is_ok();
    if ing.listen_addresses.is_empty() || !ing.listen_addresses.iter().all(|a| ip_ok(a)) {
        return None;
    }
    if ing
        .acme_email
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\' || c == '$')
    {
        return None;
    }
    let quote_list = |items: &[String]| {
        items
            .iter()
            .map(|i| format!("\"{}\"", nix_escape(i)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out = String::from("  kcore.ingress = {\n    enable = true;\n");
    out.push_str(&format!(
        "    listenAddresses = [ {} ];\n",
        quote_list(&ing.listen_addresses)
    ));
    out.push_str(&format!("    http3 = {};\n", ing.http3));
    out.push_str(&format!(
        "    acme.email = \"{}\";\n",
        nix_escape(&ing.acme_email)
    ));
    if let Some(v) = &ing.vip {
        let iface_ok = !v.interface.is_empty()
            && v.interface.len() <= 15
            && v.interface
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !ip_ok(&v.address)
            || !ip_ok(&v.source_address)
            || !v.peers.iter().all(|p| ip_ok(p))
            || !iface_ok
            || v.virtual_router_id == 0
            || v.priority == 0
        {
            return None;
        }
        out.push_str("    vip = {\n");
        out.push_str(&format!(
            "      address = \"{}\";\n",
            nix_escape(&v.address)
        ));
        out.push_str(&format!(
            "      interface = \"{}\";\n",
            nix_escape(&v.interface)
        ));
        out.push_str(&format!(
            "      virtualRouterId = {};\n",
            v.virtual_router_id
        ));
        out.push_str(&format!("      priority = {};\n", v.priority));
        out.push_str(&format!(
            "      sourceAddress = \"{}\";\n",
            nix_escape(&v.source_address)
        ));
        out.push_str(&format!("      peers = [ {} ];\n", quote_list(&v.peers)));
        out.push_str("    };\n");
    }
    out.push_str("  };\n");
    Some(out)
}

fn safe_postgresql_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && (1..=63).contains(&name.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingress(vip: bool) -> IngressNix {
        IngressNix {
            listen_addresses: vec!["203.0.113.10".into()],
            http3: true,
            acme_email: "ops@example.com".into(),
            vip: vip.then(|| IngressVipNix {
                address: "203.0.113.10".into(),
                interface: "eno1".into(),
                virtual_router_id: 61,
                priority: 100,
                source_address: "10.0.0.1".into(),
                peers: vec!["10.0.0.2".into()],
            }),
        }
    }

    #[test]
    fn with_ingress_appends_a_module_block() {
        let base = "{ config, pkgs, ... }:\n{\n  networking.hostName = \"n\";\n}\n".to_string();
        let out = with_ingress(base.clone(), Some(&ingress(true)));
        assert!(out.contains("kcore.ingress = {"), "{out}");
        assert!(out.contains("listenAddresses = [ \"203.0.113.10\" ];"));
        assert!(out.contains("virtualRouterId = 61;"));
        assert!(out.contains("peers = [ \"10.0.0.2\" ];"));
        assert!(out.trim_end().ends_with('}'));
        assert_eq!(with_ingress(base.clone(), None), base);
        assert!(!with_ingress(base, Some(&ingress(false))).contains("vip ="));
    }

    #[test]
    fn with_ingress_refuses_unsafe_values() {
        let base = "{\n}\n".to_string();
        let mut bad = ingress(true);
        bad.listen_addresses = vec!["1.2.3.4\"; evil = true; \"".into()];
        assert_eq!(with_ingress(base.clone(), Some(&bad)), base);
        let mut bad = ingress(true);
        bad.vip.as_mut().unwrap().interface = "eno1\"; x".into();
        assert_eq!(with_ingress(base.clone(), Some(&bad)), base);
        let mut bad = ingress(false);
        bad.acme_email = "a@b\" ;".into();
        assert_eq!(with_ingress(base.clone(), Some(&bad)), base);
    }
    use crate::db::VolumeRow;

    fn vm(auto_start: bool, name: &str) -> VmRow {
        VmRow {
            id: "vm-1".into(),
            name: name.into(),
            cpu: 2,
            memory_bytes: 4096 * 1024 * 1024,
            image_path: "/var/lib/kcore/images/debian.raw".into(),
            image_url: "https://example.com/debian.raw".into(),
            image_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            image_format: "raw".into(),
            image_size: 8192,
            network: "default".into(),
            auto_start,
            node_id: "node-1".into(),
            created_at: String::new(),
            runtime_state: "unknown".into(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".into(),
            storage_size_bytes: 10 * 1024 * 1024 * 1024,
            vm_ip: String::new(),
            pci_devices: String::new(),
            labels: String::new(),
        }
    }

    fn default_net() -> NetworkConfig {
        NetworkConfig {
            gateway_interface: "eno1".into(),
            external_ip: "203.0.113.10".into(),
            gateway_ip: "10.0.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
        }
    }

    #[test]
    fn generates_valid_nix() {
        let config = generate_node_config(
            &[vm(true, "web-01")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("ch-vm.vms"));
        assert!(config.contains("web-01"));
        assert!(config.contains("cores = 2"));
        assert!(config.contains("memorySize = 4096"));
        assert!(config.contains("gatewayInterface = \"eno1\""));
        assert!(config.contains("image = \"/var/lib/kcore/images/debian.raw\""));
        assert!(config.contains("imageFormat = \"raw\""));
        assert!(config.contains("storageBackend = \"filesystem\""));
        assert!(config.contains("storageSizeBytes = 10737418240"));
        assert!(config.contains("cloudInitInstanceId = \"vm-1\";"));
    }

    #[test]
    fn emits_pci_passthrough_devices() {
        let mut guest = vm(true, "gpu-guest");
        guest.pci_devices = "0000:03:00.0,0000:03:00.1".into();
        let config = generate_node_config(
            &[guest],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("address = \"0000:03:00.0\""));
        assert!(config.contains("address = \"0000:03:00.1\""));
        assert!(config.contains("ch-vm.vfio.enable = true;"));
    }

    #[test]
    fn includes_non_default_netmask_and_stopped_state() {
        let net = NetworkConfig {
            internal_netmask: "255.255.255.128".into(),
            ..default_net()
        };
        let config = generate_node_config(
            &[vm(false, "web-01")],
            "eno1",
            &net,
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("internalNetmask = \"255.255.255.128\""));
        assert!(config.contains("autoStart = false;"));
    }

    #[test]
    fn sanitizes_vm_name_for_nix_attr_key() {
        let config = generate_node_config(
            &[vm(true, "db node 01")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("virtualMachines.\"db-node-01\""));
    }

    #[test]
    fn sanitizes_special_chars_in_vm_name() {
        let config = generate_node_config(
            &[vm(true, "web\";inject")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("virtualMachines.\"web--inject\""));
        assert!(!config.contains("\";inject"));
    }

    #[test]
    fn nix_escape_handles_quotes_and_backslashes() {
        assert_eq!(nix_escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(nix_escape(r"a\b"), r"a\\b");
        assert_eq!(nix_escape("a${b}"), "a\\${b}");
    }

    #[test]
    fn image_path_with_special_chars_is_escaped() {
        let mut v = vm(true, "evil");
        v.image_path = r#"/images/foo"${bar}.raw"#.into();
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains(r#"image = "/images/foo\"\${bar}.raw";"#));
        // The raw `${` is escaped to `\${`, preventing Nix interpolation.
        assert!(!config.contains("image = \"/images/foo\"${bar}.raw\";"));
    }

    #[test]
    fn image_sha256_is_rendered_only_when_well_formed() {
        let render = |sha: &str| {
            let mut v = vm(true, "sha");
            v.image_sha256 = sha.into();
            generate_node_config(
                &[v],
                "eno1",
                &default_net(),
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
            )
        };
        let good = "A".repeat(32) + &"b".repeat(32);
        let config = render(&good);
        assert!(
            config.contains(&format!("imageSha256 = \"{}\";", good.to_ascii_lowercase())),
            "{config}"
        );
        for bad in [
            "",
            "abc",
            &"g".repeat(64),
            &format!("{}\"; evil = 1; x = \"", "a".repeat(64)),
        ] {
            let config = render(bad);
            assert!(!config.contains("imageSha256"), "{bad:?} -> {config}");
        }
    }

    #[test]
    fn image_sha256_is_withheld_from_nodes_whose_module_lacks_it() {
        let render = |features: &ModuleFeatures| {
            let mut v = vm(true, "sha");
            v.image_sha256 = "a".repeat(64);
            generate_node_config_inner(
                &[v],
                "eno1",
                &default_net(),
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashSet::new(),
                &[],
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                features,
                &[],
            )
        };
        assert!(render(&ModuleFeatures::all()).contains("imageSha256"));
        assert!(!render(&ModuleFeatures::default()).contains("imageSha256"));

        let reported = ModuleFeatures::from_reported(&["cloudHypervisorPatches", "imageSha256"]);
        assert!(reported.image_sha256);
        assert!(!reported.vgpu_profile);
        assert!(!ModuleFeatures::from_reported::<&str>(&[]).image_sha256);
        assert!(ModuleFeatures::from_reported(&[ModuleFeatures::VGPU_PROFILE]).vgpu_profile);
        assert!(!ModuleFeatures::from_reported(&["imagesha256"]).image_sha256);
    }

    #[test]
    fn vgpu_profile_is_rendered_only_when_the_module_reports_it() {
        let mut guest = vm(true, "slice-guest");
        guest.pci_devices = "0000:af:00.4".into();
        let slices = [VgpuSliceNix {
            vm_id: guest.id.clone(),
            address: "0000:af:00.4".into(),
            profile: "A2-4C".into(),
            host_address: "0000:af:00.0".into(),
        }];
        let render = |features: &ModuleFeatures| {
            generate_node_config_inner(
                &[guest.clone()],
                "eno1",
                &default_net(),
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashSet::new(),
                &[],
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                features,
                &slices,
            )
        };
        let enabled = render(&ModuleFeatures::all());
        assert!(enabled.contains("vgpuProfile = \"A2-4C\""));
        assert!(enabled.contains("ch-vm.vgpu.enable = true;"));
        assert!(enabled.contains("0000:af:00.0"));
        assert!(!enabled.contains("ch-vm.vfio.enable"));
        let baseline = render(&ModuleFeatures::default());
        assert!(!baseline.contains("vgpuProfile"));
        assert!(!baseline.contains("ch-vm.vgpu.enable"));
        assert!(baseline.contains("address = \"0000:af:00.4\""));
    }

    #[test]
    fn image_format_is_rendered_for_qcow2() {
        let mut v = vm(true, "qcow");
        v.image_format = "qcow2".into();
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("imageFormat = \"qcow2\";"));
    }

    #[test]
    fn network_values_are_escaped() {
        let net = NetworkConfig {
            gateway_interface: "eno1\"".into(),
            external_ip: "1.2.3.4\"".into(),
            gateway_ip: "10.0.0.1\\".into(),
            internal_netmask: "255.255.255.0".into(),
        };
        let config = generate_node_config(
            &[],
            "eno1\"",
            &net,
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains(r#"gatewayInterface = "eno1\"";"#));
        assert!(config.contains(r#"externalIP = "1.2.3.4\"";"#));
        assert!(config.contains(r#"gatewayIP = "10.0.0.1\\";"#));
    }

    #[test]
    fn renders_custom_networks() {
        let networks = vec![NetworkRow {
            name: "frontend".into(),
            external_ip: "198.51.100.5".into(),
            gateway_ip: "10.240.10.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "nat".into(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        }];
        let config = generate_node_config(
            &[],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("networks.\"frontend\""));
        assert!(config.contains("gatewayIP = \"10.240.10.1\";"));
    }

    #[test]
    fn renders_network_port_forwarding() {
        let networks = vec![NetworkRow {
            name: "frontend".into(),
            external_ip: "198.51.100.5".into(),
            gateway_ip: "10.240.10.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: "80,443,8080".into(),
            allowed_udp_ports: "53".into(),
            vlan_id: 0,
            network_type: "nat".into(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        }];
        let config = generate_node_config(
            &[],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("allowedTCPPorts = [ 80 443 8080 ];"));
        assert!(config.contains("allowedUDPPorts = [ 53 ];"));
    }

    #[test]
    fn renders_network_with_vlan_id() {
        let v = vm(true, "web-01");
        let net = NetworkRow {
            name: "production".to_string(),
            external_ip: "203.0.113.10".to_string(),
            gateway_ip: "10.100.0.1".to_string(),
            internal_netmask: "255.255.255.0".to_string(),
            node_id: "node-1".to_string(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 100,
            network_type: "nat".into(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        };
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[net],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("vlanId = 100"), "should contain vlanId");
        assert!(config.contains("production"), "should contain network name");
    }

    #[test]
    fn injects_ssh_keys_into_cloud_init() {
        let v = vm(true, "web-01");
        let mut keys = std::collections::HashMap::new();
        keys.insert(
            "vm-1".to_string(),
            vec!["ssh-rsa AAAAB3... user@host".to_string()],
        );
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &keys,
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("cloudInitUserConfigFile"));
        assert!(config.contains("ssh_authorized_keys"));
        assert!(config.contains("lock_passwd: true"));
        assert!(config.contains("ssh_pwauth: false"));
    }

    #[test]
    fn custom_cloud_init_still_gets_guest_ops_keys() {
        let mut v = vm(true, "web-01");
        v.cloud_init_user_data = "#cloud-config\nhostname: web-01\n".to_string();
        let ops = vec!["ssh-ed25519 AAAAops kcore-guest-ops".to_string()];
        let none = std::collections::HashMap::new();
        let config = generate_node_config_inner(
            &[v.clone()],
            "eno1",
            &default_net(),
            &[],
            &none,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &[],
            &ops,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &ModuleFeatures::all(),
            &[],
        );
        assert!(config.contains("cloudInitUserConfigFile"));
        assert!(config.contains("cloudInitGuestOpsKeys"));
        assert!(config.contains("ssh-ed25519 AAAAops kcore-guest-ops"));

        let baseline = generate_node_config_inner(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &none,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &[],
            &ops,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &ModuleFeatures::default(),
            &[],
        );
        assert!(
            !baseline.contains("cloudInitGuestOpsKeys"),
            "an older module has no cloudInitGuestOpsKeys option; emitting it fails the whole evaluation"
        );
    }

    #[test]
    fn renders_bridge_network_without_masquerade() {
        let networks = vec![NetworkRow {
            name: "bridged".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "0.0.0.0".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "bridge".into(),
            enable_outbound_nat: false,
            vni: 0,
            next_ip: 2,
        }];
        let config = generate_node_config(
            &[],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("networkType = \"bridge\""));
        assert!(config.contains("enableOutboundNat = false"));
        assert!(!config.contains("vni ="));
    }

    #[test]
    fn renders_vxlan_network_with_peers() {
        let networks = vec![NetworkRow {
            name: "overlay".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.200.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: true,
            vni: 10042,
            next_ip: 2,
        }];
        let mut vxlan = std::collections::HashMap::new();
        vxlan.insert(
            "overlay".to_string(),
            VxlanMeta {
                vni: 10042,
                peers: vec!["192.168.1.20".into(), "192.168.1.30".into()],
                local_ip: "192.168.1.10".into(),
            },
        );
        let config = generate_node_config(
            &[],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &vxlan,
        );
        assert!(config.contains("networkType = \"vxlan\""));
        assert!(config.contains("vni = 10042"));
        assert!(config.contains("\"192.168.1.20\""));
        assert!(config.contains("\"192.168.1.30\""));
        assert!(config.contains("vxlanLocalIp = \"192.168.1.10\""));
    }

    #[test]
    fn renders_vxlan_vm_with_static_ip() {
        let mut v = vm(true, "vxvm");
        v.network = "overlay".into();
        v.vm_ip = "10.200.0.5".into();

        let networks = vec![NetworkRow {
            name: "overlay".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.200.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: true,
            vni: 10042,
            next_ip: 6,
        }];
        let mut vxlan = std::collections::HashMap::new();
        vxlan.insert(
            "overlay".to_string(),
            VxlanMeta {
                vni: 10042,
                peers: vec![],
                local_ip: "192.168.1.10".into(),
            },
        );
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &vxlan,
        );
        assert!(
            config.contains("cloudInitNetworkConfigFile"),
            "should have static network config"
        );
        assert!(config.contains("name: \\\"e*\\\""));
        assert!(config.contains("dhcp4: false"));
        assert!(config.contains("10.200.0.5/24"));
        assert!(config.contains("gateway4"));
    }

    #[test]
    fn extra_nics_are_emitted_and_skip_the_single_nic_cloud_init_file() {
        let mut v = vm(true, "web");
        v.id = "vm-web".into();
        v.network = "frontend".into();
        v.vm_ip = "10.240.0.10".into();
        let networks = vec![NetworkRow {
            name: "frontend".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.240.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: true,
            vni: 1,
            next_ip: 11,
        }];
        let mut extras = std::collections::HashMap::new();
        extras.insert(
            v.id.clone(),
            vec![VmNicRow {
                vm_id: v.id.clone(),
                position: 1,
                network: "backend".into(),
                mac_address: String::new(),
                model: "virtio".into(),
                ip_address: "10.241.0.10".into(),
            }],
        );
        let config = generate_node_config_inner(
            &[v],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &[],
            &[],
            &extras,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &ModuleFeatures::all(),
            &[],
        );
        assert!(config.contains("extraNics"));
        assert!(!config.contains("eastWestFirewall"));
        assert!(!config.contains("dhcpReservedIPv6"));
        assert!(config.contains("backend"));
        assert!(config.contains("10.241.0.10"));
        assert!(
            !config.contains("cloudInitNetworkConfigFile"),
            "multi-NIC cloud-init is rendered by the module:\n{config}"
        );
    }

    #[test]
    fn ipv6_and_east_west_are_emitted_only_when_the_policy_sets_them() {
        let mut v = vm(true, "web");
        v.id = "vm-web".into();
        v.network = "frontend".into();
        v.vm_ip = "10.240.0.10".into();
        let networks = vec![NetworkRow {
            name: "frontend".into(),
            external_ip: "0.0.0.0".into(),
            gateway_ip: "10.240.0.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "vxlan".into(),
            enable_outbound_nat: true,
            vni: 1,
            next_ip: 11,
        }];
        let mut policies = std::collections::HashMap::new();
        policies.insert(
            "frontend".into(),
            crate::db::NetworkPolicyRow {
                node_id: "node-1".into(),
                name: "frontend".into(),
                east_west: true,
                ipv6_prefix: "fd00:10:240::/64".into(),
                ipv6_gateway: "fd00:10:240::1".into(),
                ipv6_next: 3,
            },
        );
        let mut addresses = std::collections::HashMap::new();
        addresses.insert(
            v.id.clone(),
            vec![crate::db::VmIpv6Row {
                vm_id: v.id.clone(),
                position: 0,
                address: "fd00:10:240::2".into(),
            }],
        );
        let config = generate_node_config_inner(
            &[v],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &[],
            &[],
            &std::collections::HashMap::new(),
            &policies,
            &addresses,
            &ModuleFeatures::all(),
            &[],
        );
        assert!(config.contains("eastWestFirewall = true;"));
        assert!(config.contains("ipv6Prefix = \"fd00:10:240::/64\";"));
        assert!(config.contains("ipv6Gateway = \"fd00:10:240::1\";"));
        assert!(config.contains("dhcpReservedIPv6 = \"fd00:10:240::2\";"));
        assert!(config.contains("fd00:10:240::2/64"));
    }

    #[test]
    fn emits_lvm_vg_name_when_vm_uses_lvm() {
        let mut v = vm(true, "lvm-vm");
        v.storage_backend = "lvm".into();
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(
            config.contains("lvmVgName = \"vg_kcore\";"),
            "should contain lvmVgName"
        );
    }

    #[test]
    fn omits_lvm_vg_name_for_filesystem_backend() {
        let config = generate_node_config(
            &[vm(true, "fs-vm")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !config.contains("lvmVgName"),
            "should not contain lvmVgName"
        );
    }

    #[test]
    fn emits_zfs_pool_name_when_vm_uses_zfs() {
        let mut v = vm(true, "zfs-vm");
        v.storage_backend = "zfs".into();
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(
            config.contains("zfsPoolName = \"tank0\";"),
            "should contain zfsPoolName"
        );
        assert!(
            !config.contains("lvmVgName"),
            "ZFS VM should not contain lvmVgName"
        );
    }

    #[test]
    fn clone_from_snapshot_boot_lists_rbd_in_data_disks() {
        let mut v = vm(true, "app");
        v.id = "vm-1".into();
        v.storage_backend = "ceph".into();
        let root = VolumeRow::new_root("vm-1", "app", 10 * 1024 * 1024 * 1024);
        let mut parent = VolumeRow::new_data("pgdata", 20 * 1024 * 1024 * 1024);
        parent.vm_id = v.id.clone();
        parent.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        parent.slot = 1;
        let mut clone = VolumeRow::new_data("pgdata-clone", parent.size_bytes);
        clone.parent_snapshot_id = "snap-t0".into();
        clone.vm_id = "vm-scratch".into();
        clone.attach_state = VolumeRow::ATTACH_ATTACHED.into();
        clone.slot = 1;
        clone.image = "kcore-vol-clone01".into();
        let clone_serial = clone.serial.clone();
        let mut scratch = vm(false, "scratch");
        scratch.id = "vm-scratch".into();
        scratch.storage_backend = "ceph".into();
        let scratch_root = VolumeRow::new_root("vm-scratch", "scratch", 10 * 1024 * 1024 * 1024);
        let config = generate_node_config_inner(
            &[v, scratch],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashSet::new(),
            &[root, parent, clone, scratch_root],
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &ModuleFeatures::all(),
            &[],
        );
        assert!(
            config.contains("rbdImage = \"kcore-vol-clone01\""),
            "clone attached as scratch VM data disk must appear in nixgen:\n{config}"
        );
        assert!(config.contains("dataDisks = ["));
        assert!(config.contains(&format!("serial = \"{clone_serial}\"")));
    }

    #[test]
    fn emits_single_rbd_image_when_vm_uses_ceph() {
        let mut v = vm(true, "ceph-vm");
        v.id = "vm-42".into();
        v.storage_backend = "ceph".into();
        let config = generate_node_config(
            &[v],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(config.contains("storageBackend = \"ceph\";"));
        let needle = "rbdImage = \"kcore-vm-42\";";
        assert!(config.contains(needle), "missing rbdImage in:\n{config}");
        assert_eq!(
            config.matches(needle).count(),
            1,
            "rbdImage must be emitted exactly once"
        );
    }

    #[test]
    fn omits_rbd_image_for_filesystem_backend() {
        let config = generate_node_config(
            &[vm(true, "fs-vm")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(!config.contains("rbdImage"));
    }

    #[test]
    fn omits_zfs_pool_name_for_filesystem_backend() {
        let config = generate_node_config(
            &[vm(true, "fs-vm")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        assert!(
            !config.contains("zfsPoolName"),
            "should not contain zfsPoolName"
        );
    }

    #[test]
    fn netmask_to_cidr_converts_common_masks() {
        assert_eq!(netmask_to_cidr("255.255.255.0"), 24);
        assert_eq!(netmask_to_cidr("255.255.0.0"), 16);
        assert_eq!(netmask_to_cidr("255.255.255.128"), 25);
    }

    #[test]
    fn renders_security_group_rules_for_network() {
        let networks = vec![NetworkRow {
            name: "private".into(),
            external_ip: "198.51.100.5".into(),
            gateway_ip: "10.240.10.1".into(),
            internal_netmask: "255.255.255.0".into(),
            node_id: "node-1".into(),
            allowed_tcp_ports: String::new(),
            allowed_udp_ports: String::new(),
            vlan_id: 0,
            network_type: "nat".into(),
            enable_outbound_nat: true,
            vni: 0,
            next_ip: 2,
        }];
        let mut sg_rules = std::collections::HashMap::new();
        sg_rules.insert(
            "private".to_string(),
            vec![SecurityGroupResolvedRule {
                protocol: "tcp".to_string(),
                host_port: 8443,
                target_port: 443,
                source_cidr: "0.0.0.0/0".to_string(),
                target_ip: "10.240.10.22".to_string(),
                enable_dnat: true,
            }],
        );
        let config = generate_node_config_with_security_groups(
            &[],
            "eno1",
            &default_net(),
            &networks,
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &sg_rules,
        );
        assert!(config.contains("securityGroupRules = ["));
        assert!(config.contains("hostPort = 8443;"));
        assert!(config.contains("targetIp = \"10.240.10.22\";"));
        assert!(config.contains("enableDnat = true;"));
    }

    #[test]
    fn postgresql_block_uses_nixos_package_and_drops_unsafe_values() {
        let base = generate_node_config(
            &[vm(true, "web-01")],
            "eno1",
            &default_net(),
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
        );
        let with = with_postgresql(
            base.clone(),
            Some(&PostgresqlNix {
                database: "app".into(),
                package: "postgresql".into(),
                port: 5432,
            }),
        );
        assert!(with.contains("ch-vm.vms"));
        assert!(with.contains("services.postgresql"));
        assert!(with.contains("package = pkgs.postgresql;"));
        assert!(with.contains("ensureDatabases = [ \"app\" ];"));
        assert!(with.trim_end().ends_with('}'));

        let injected = with_postgresql(
            base.clone(),
            Some(&PostgresqlNix {
                database: "app\"; builtins.trace \"x\"".into(),
                package: "postgresql".into(),
                port: 5432,
            }),
        );
        assert_eq!(injected, base);
        let bad_pkg = with_postgresql(
            base.clone(),
            Some(&PostgresqlNix {
                database: "app".into(),
                package: "postgresql; import /tmp/evil".into(),
                port: 5432,
            }),
        );
        assert_eq!(bad_pkg, base);
        assert_eq!(
            with_postgresql(base, None),
            generate_node_config(
                &[vm(true, "web-01")],
                "eno1",
                &default_net(),
                &[],
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
            )
        );
    }
}

/// Kubernetes cluster node configuration parameters.
#[derive(Debug, Clone)]
pub struct KubernetesNodeConfig {
    pub role: String,
    pub cluster_name: String,
    pub cluster_cidr: String,
    pub service_cidr: String,
    pub cluster_dns: String,
    pub advertise_address: String,
    pub iface: String,
    pub api_server_endpoint: String,
    pub initial_cluster: bool,
    pub etcd_initial_cluster: String,
    pub cni: String,
    pub cilium_mode: String,
    pub cilium_mtu: i32,
    pub cilium_tunnel: String,
    pub cilium_auto_direct_routes: bool,
    pub cilium_lb_ip_pools: Vec<String>,
    /// Node interfaces that announce LoadBalancer IPs. Empty on an extra
    /// cluster, which does not install Cilium.
    pub cilium_l2_interfaces: Vec<String>,
    pub ha_enabled: bool,
    pub vip_address: String,
    pub vip_interface: String,
    pub kubeconfig_data: String,
    /// Control-plane kubelets also register the worker role and stay schedulable.
    pub schedule_on_control_plane: bool,
    /// Applied by the control plane after the API is ready. Empty skips it.
    /// LVM mode leaves this empty: that mode creates a thin pool and does
    /// not install a provisioner.
    pub provisioner_manifest: String,
    /// Set for a cluster that shares the node with the primary one.
    /// The primary cluster keeps the module defaults and leaves this empty.
    pub instance: Option<KubernetesInstanceLayout>,
}

/// Ports and per-node pod CIDR for a cluster that is not the host's primary.
#[derive(Debug, Clone)]
pub struct KubernetesInstanceLayout {
    pub api_server_port: u16,
    pub etcd_client_port: u16,
    pub etcd_peer_port: u16,
    pub kubelet_port: u16,
    pub kubelet_healthz_port: u16,
    pub controller_manager_port: u16,
    pub scheduler_port: u16,
    pub virtual_router_id: u16,
    pub pod_subnet: String,
}

/// Generate the kcore.kubernetes NixOS module configuration for a node.
///
/// This produces a Nix attribute set that will be merged into the node's
/// configuration.nix, enabling the Kubernetes services (etcd, kube-apiserver,
/// controller-manager, scheduler, kubelet) based on the node's role.
pub fn generate_kubernetes_node_config(k8s: &KubernetesNodeConfig) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  kcore.kubernetes = {\n");
    out.push_str("    enable = true;\n");
    out.push_str(&format!("    role = \"{}\";\n", nix_escape(&k8s.role)));
    out.push_str(&format!(
        "    clusterName = \"{}\";\n",
        nix_escape(&k8s.cluster_name)
    ));
    out.push_str(&format!(
        "    clusterCidr = \"{}\";\n",
        nix_escape(&k8s.cluster_cidr)
    ));
    out.push_str(&format!(
        "    serviceCidr = \"{}\";\n",
        nix_escape(&k8s.service_cidr)
    ));
    out.push_str(&format!(
        "    clusterDns = \"{}\";\n",
        nix_escape(&k8s.cluster_dns)
    ));
    out.push_str(&format!(
        "    advertiseAddress = \"{}\";\n",
        nix_escape(&k8s.advertise_address)
    ));
    out.push_str(&format!("    iface = \"{}\";\n", nix_escape(&k8s.iface)));
    out.push_str(&format!(
        "    apiServerEndpoint = \"{}\";\n",
        nix_escape(&k8s.api_server_endpoint)
    ));
    out.push_str(&format!("    initialCluster = {};\n", k8s.initial_cluster));
    out.push_str(&format!(
        "    scheduleOnControlPlane = {};\n",
        k8s.schedule_on_control_plane
    ));

    if k8s.role == "control-plane" {
        out.push_str(&format!(
            "    etcdInitialCluster = \"{}\";\n",
            nix_escape(&k8s.etcd_initial_cluster)
        ));
    }

    // Extra clusters use a private bridge CNI. The submodule has no Cilium
    // options, and a second Cilium agent would fight the primary one for BPF.
    if k8s.instance.is_none() {
        out.push_str(&format!("    cni = \"{}\";\n", nix_escape(&k8s.cni)));
        out.push_str(&format!(
            "    ciliumMode = \"{}\";\n",
            nix_escape(&k8s.cilium_mode)
        ));
        out.push_str(&format!("    ciliumMtu = {};\n", k8s.cilium_mtu));
        out.push_str(&format!(
            "    ciliumTunnel = \"{}\";\n",
            nix_escape(&k8s.cilium_tunnel)
        ));
        out.push_str(&format!(
            "    ciliumAutoDirectNodeRoutes = {};\n",
            k8s.cilium_auto_direct_routes
        ));

        if !k8s.cilium_lb_ip_pools.is_empty() {
            out.push_str("    ciliumLoadBalancerIpPools = [\n");
            for pool in &k8s.cilium_lb_ip_pools {
                out.push_str(&format!("      \"{}\"\n", nix_escape(pool)));
            }
            out.push_str("    ];\n");
        }
        if !k8s.cilium_l2_interfaces.is_empty() {
            out.push_str("    ciliumL2Interfaces = [\n");
            for iface in &k8s.cilium_l2_interfaces {
                out.push_str(&format!("      \"{}\"\n", nix_escape(iface)));
            }
            out.push_str("    ];\n");
        }
    }

    out.push_str(&format!("    haEnabled = {};\n", k8s.ha_enabled));
    if k8s.ha_enabled {
        out.push_str(&format!(
            "    vipAddress = \"{}\";\n",
            nix_escape(&k8s.vip_address)
        ));
        out.push_str(&format!(
            "    vipInterface = \"{}\";\n",
            nix_escape(&k8s.vip_interface)
        ));
    }

    if let Some(instance) = &k8s.instance {
        out.push_str(&format!(
            "    apiServerPort = {};\n",
            instance.api_server_port
        ));
        out.push_str(&format!(
            "    etcdClientPort = {};\n",
            instance.etcd_client_port
        ));
        out.push_str(&format!(
            "    etcdPeerPort = {};\n",
            instance.etcd_peer_port
        ));
        out.push_str(&format!("    kubeletPort = {};\n", instance.kubelet_port));
        out.push_str(&format!(
            "    kubeletHealthzPort = {};\n",
            instance.kubelet_healthz_port
        ));
        out.push_str(&format!(
            "    controllerManagerPort = {};\n",
            instance.controller_manager_port
        ));
        out.push_str(&format!(
            "    schedulerPort = {};\n",
            instance.scheduler_port
        ));
        out.push_str(&format!(
            "    virtualRouterId = {};\n",
            instance.virtual_router_id
        ));
        if !instance.pod_subnet.is_empty() {
            out.push_str(&format!(
                "    podSubnet = \"{}\";\n",
                nix_escape(&instance.pod_subnet)
            ));
        }
    }

    if !k8s.provisioner_manifest.is_empty() {
        out.push_str(&format!(
            "    provisionerManifest = {};\n",
            nix_multiline(&k8s.provisioner_manifest)
        ));
    }

    if !k8s.kubeconfig_data.is_empty() {
        out.push_str(&format!(
            "    kubeconfigData = ''{}'';\n",
            k8s.kubeconfig_data
        ));
    }

    out.push_str("  };\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod kubernetes_tests {
    use super::*;

    #[test]
    fn test_generate_kubernetes_control_plane_config() {
        let config = KubernetesNodeConfig {
            role: "control-plane".to_string(),
            cluster_name: "kcore-k8s".to_string(),
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            advertise_address: "192.168.40.105".to_string(),
            iface: "enp6s0".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            initial_cluster: true,
            etcd_initial_cluster: "node-105=https://192.168.40.105:2380".to_string(),
            cni: "cilium".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: vec!["192.168.40.220/29".to_string()],
            cilium_l2_interfaces: vec!["enp6s0".to_string()],
            ha_enabled: true,
            vip_address: "192.168.40.200".to_string(),
            vip_interface: "enp6s0".to_string(),
            kubeconfig_data: String::new(),
            schedule_on_control_plane: true,
            provisioner_manifest: String::new(),
            instance: None,
        };

        let nix = generate_kubernetes_node_config(&config);
        assert!(nix.contains("enable = true"));
        assert!(nix.contains("role = \"control-plane\""));
        assert!(nix.contains("scheduleOnControlPlane = true"));
        assert!(nix.contains("haEnabled = true"));
        assert!(nix.contains("initialCluster = true"));
        assert!(nix.contains("etcdInitialCluster"));
    }

    #[test]
    fn test_generate_kubernetes_worker_config() {
        let config = KubernetesNodeConfig {
            role: "worker".to_string(),
            cluster_name: "kcore-k8s".to_string(),
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            advertise_address: "192.168.40.110".to_string(),
            iface: "enp6s0".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            initial_cluster: false,
            etcd_initial_cluster: String::new(),
            cni: "cilium".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: vec![],
            cilium_l2_interfaces: vec![],
            ha_enabled: false,
            vip_address: String::new(),
            vip_interface: String::new(),
            kubeconfig_data: String::new(),
            schedule_on_control_plane: false,
            provisioner_manifest: String::new(),
            instance: None,
        };

        let nix = generate_kubernetes_node_config(&config);
        assert!(nix.contains("enable = true"));
        assert!(nix.contains("role = \"worker\""));
        assert!(nix.contains("haEnabled = false"));
        assert!(!nix.contains("etcdInitialCluster"));
    }

    #[test]
    fn pki_options_stay_inside_kubernetes_attrset() {
        use crate::kubernetes_orchestrator::ClusterNode;
        use crate::kubernetes_pki::KubernetesPki;
        use std::collections::HashMap;

        let node = ClusterNode {
            node_id: "node-107".to_string(),
            hostname: "node-107".to_string(),
            ip_address: "192.168.40.107".to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: true,
            role: "control-plane".to_string(),
            kubelet_kubeconfig: String::new(),
        };
        let peer = "-----BEGIN CERTIFICATE-----\nMIIBPEER\n-----END CERTIFICATE-----\n".to_string();
        let server =
            "-----BEGIN CERTIFICATE-----\nMIIBSERVER\n-----END CERTIFICATE-----\n".to_string();
        let mut etcd_node_certs = HashMap::new();
        etcd_node_certs.insert(
            node.node_id.clone(),
            (server.clone(), "server-key".to_string()),
        );
        let mut etcd_node_peer_certs = HashMap::new();
        etcd_node_peer_certs.insert(node.node_id.clone(), (peer.clone(), "peer-key".to_string()));
        let pki = KubernetesPki {
            ca_cert: "CA".to_string(),
            ca_key: "CAKEY".to_string(),
            sa_key: "SA".to_string(),
            sa_pub: "SAPUB".to_string(),
            etcd_ca_cert: "ETCDCA".to_string(),
            etcd_ca_key: "ETCDCAKEY".to_string(),
            apiserver_cert: "API".to_string(),
            apiserver_key: "APIKEY".to_string(),
            apiserver_kubelet_client_cert: "AKC".to_string(),
            apiserver_kubelet_client_key: "AKCKEY".to_string(),
            apiserver_etcd_client_cert: "AEC".to_string(),
            apiserver_etcd_client_key: "AECKEY".to_string(),
            etcd_node_certs,
            etcd_node_peer_certs,
            admin_kubeconfig: "apiVersion: v1\nkind: Config\n".to_string(),
        };

        let nix = generate_kubernetes_config(
            "lab",
            &node,
            &pki,
            "10.244.0.0/16",
            "10.96.0.0/12",
            &server,
            "server-key",
            &peer,
            "peer-key",
            std::slice::from_ref(&node),
            &KubernetesRenderSettings {
                api_server_endpoint: "192.168.40.200:6443".to_string(),
                cluster_dns: "10.96.0.10".to_string(),
                ha_enabled: true,
                vip_address: "192.168.40.200".to_string(),
                vip_interface: "enp6s0".to_string(),
                cilium_mode: "native-routing".to_string(),
                cilium_mtu: 1500,
                cilium_tunnel: "disabled".to_string(),
                cilium_auto_direct_routes: true,
                cilium_lb_ip_pools: vec!["192.168.40.220/29".to_string()],
                schedule_on_control_plane: false,
                provisioner_manifest: String::new(),
            },
        )
        .unwrap();

        let close = nix.rfind("\n  };\n").expect("kubernetes attrset close");
        let peer_at = nix.find("etcdPeerCrt").expect("etcdPeerCrt option");
        assert!(
            peer_at < close,
            "etcdPeerCrt must be inside kcore.kubernetes"
        );
        assert!(nix.contains("etcdPeerKey"));
        assert!(nix.contains("MIIBPEER"));
        assert!(nix.contains("node-107=https://192.168.40.107:2380"));
        assert!(nix.contains("apiServerEndpoint = \"192.168.40.200:6443\""));
        assert!(nix.contains("haEnabled = true"));
        assert!(nix.contains("vipAddress = \"192.168.40.200\""));
        assert!(nix.contains("ciliumL2Interfaces"));
        assert!(nix.contains("\"enp6s0\""));
        assert!(!nix.contains("https://https://"));
        assert!(!nix.contains("environment.etc"));
        assert!(nix.ends_with("  };\n}\n"));
        assert_eq!(nix.matches("kubeconfigData").count(), 1);
    }

    #[test]
    fn colocated_clusters_keep_primary_ports_and_nest_the_extra() {
        use crate::kubernetes_orchestrator::ClusterNode;
        use crate::kubernetes_pki::KubernetesPki;
        use std::collections::HashMap;

        let node = ClusterNode {
            node_id: "node-105".to_string(),
            hostname: "node-105".to_string(),
            ip_address: "192.168.40.105".to_string(),
            iface: "br0".to_string(),
            initial_cluster: true,
            role: "control-plane".to_string(),
            kubelet_kubeconfig: String::new(),
        };
        let pki = KubernetesPki {
            ca_cert: "CA".to_string(),
            ca_key: "CAKEY".to_string(),
            sa_key: "SA".to_string(),
            sa_pub: "SAPUB".to_string(),
            etcd_ca_cert: "ETCDCA".to_string(),
            etcd_ca_key: "ETCDCAKEY".to_string(),
            apiserver_cert: "API".to_string(),
            apiserver_key: "APIKEY".to_string(),
            apiserver_kubelet_client_cert: "AKC".to_string(),
            apiserver_kubelet_client_key: "AKCKEY".to_string(),
            apiserver_etcd_client_cert: "AEC".to_string(),
            apiserver_etcd_client_key: "AECKEY".to_string(),
            etcd_node_certs: HashMap::from([(
                node.node_id.clone(),
                ("server".to_string(), "server-key".to_string()),
            )]),
            etcd_node_peer_certs: HashMap::from([(
                node.node_id.clone(),
                ("peer".to_string(), "peer-key".to_string()),
            )]),
            admin_kubeconfig: "apiVersion: v1\n".to_string(),
        };
        let primary = KubernetesRenderSettings {
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            ha_enabled: true,
            vip_address: "192.168.40.200".to_string(),
            vip_interface: "br0".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: vec!["192.168.40.220/29".to_string()],
            schedule_on_control_plane: false,
            provisioner_manifest: String::new(),
        };
        let extra = KubernetesRenderSettings {
            api_server_endpoint: "192.168.40.201:7443".to_string(),
            cluster_dns: "10.112.0.10".to_string(),
            ha_enabled: true,
            vip_address: "192.168.40.201".to_string(),
            vip_interface: "br0".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: vec![],
            schedule_on_control_plane: true,
            provisioner_manifest:
                "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: local-path-config\n"
                    .to_string(),
        };
        let nix = generate_colocated_kubernetes_config(&[
            KubernetesMemberRender {
                cluster_name: "kcore-b",
                node: &node,
                pki: &pki,
                cluster_cidr: "10.245.0.0/16",
                service_cidr: "10.112.0.0/12",
                etcd_server_cert: "server",
                etcd_server_key: "server-key",
                etcd_peer_cert: "peer",
                etcd_peer_key: "peer-key",
                all_nodes: std::slice::from_ref(&node),
                settings: &extra,
                pod_subnet: node_pod_subnet("10.245.0.0/16", 0),
            },
            KubernetesMemberRender {
                cluster_name: "kcore-prod",
                node: &node,
                pki: &pki,
                cluster_cidr: "10.244.0.0/16",
                service_cidr: "10.96.0.0/12",
                etcd_server_cert: "server",
                etcd_server_key: "server-key",
                etcd_peer_cert: "peer",
                etcd_peer_key: "peer-key",
                all_nodes: std::slice::from_ref(&node),
                settings: &primary,
                pod_subnet: String::new(),
            },
        ])
        .unwrap();

        let extra_at = nix.find("additionalClusters.\"kcore-b\"").unwrap();
        let primary_etcd = nix.find("node-105=https://192.168.40.105:2380").unwrap();
        assert!(primary_etcd < extra_at);
        assert!(nix.contains("node-105=https://192.168.40.105:3380"));
        assert!(nix.contains("apiServerPort = 7443"));
        assert!(nix.contains("podSubnet = \"10.245.0.0/24\""));
        assert!(nix.contains("virtualRouterId = 41"));
        assert!(nix.contains("clusterName = \"kcore-prod\""));
        let extra = &nix[extra_at..];
        assert!(!extra.contains("ciliumMode"));
        assert!(!extra.contains("ciliumL2Interfaces"));
        assert!(nix[..extra_at].contains("ciliumL2Interfaces"));
        assert!(nix[..extra_at].contains("\"br0\""));
        assert!(!extra.contains("cni ="));
        assert!(extra.contains("scheduleOnControlPlane = true"));
        assert!(extra.contains("provisionerManifest"));
        assert!(extra.contains("name: local-path-config"));
        assert!(!nix[..extra_at].contains("provisionerManifest"));
        assert!(nix.contains("node-105=https://192.168.40.105:3380"));
        assert!(nix.ends_with("  };\n}\n"));
        assert_eq!(
            kubernetes_listen_ports("192.168.40.201:7443").etcd_client,
            3379
        );
    }

    #[test]
    fn cilium_l2_interfaces_are_sorted_unique_and_checked() {
        use crate::kubernetes_orchestrator::ClusterNode;
        let nodes = [
            ClusterNode {
                node_id: "b".into(),
                hostname: "b".into(),
                ip_address: "192.168.40.107".into(),
                iface: "enp6s0".into(),
                initial_cluster: false,
                role: "worker".into(),
                kubelet_kubeconfig: String::new(),
            },
            ClusterNode {
                node_id: "a".into(),
                hostname: "a".into(),
                ip_address: "192.168.40.105".into(),
                iface: String::new(),
                initial_cluster: true,
                role: "control-plane".into(),
                kubelet_kubeconfig: String::new(),
            },
            ClusterNode {
                node_id: "c".into(),
                hostname: "c".into(),
                ip_address: "192.168.40.103".into(),
                iface: "eth0".into(),
                initial_cluster: false,
                role: "worker".into(),
                kubelet_kubeconfig: String::new(),
            },
        ];
        assert_eq!(
            cilium_l2_interfaces(&nodes).unwrap(),
            vec!["enp6s0".to_string(), "eth0".to_string()]
        );
        assert!(validate_cilium_pool("192.168.40.220/29").is_ok());
        assert!(validate_cilium_pool("not-a-cidr").is_err());
        assert!(validate_cilium_pool("192.168.40.220/33").is_err());
        assert!(validate_cilium_iface("enp6s0").is_ok());
        assert!(validate_cilium_iface("bad iface").is_err());
    }
}

fn validate_cilium_iface(iface: &str) -> anyhow::Result<()> {
    let ok = !iface.is_empty()
        && iface.len() <= 15
        && !iface.starts_with('-')
        && iface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
    if ok {
        Ok(())
    } else {
        anyhow::bail!("cilium interface {iface:?} is not a device name");
    }
}

fn validate_cilium_pool(pool: &str) -> anyhow::Result<()> {
    let Some((addr, prefix)) = pool.split_once('/') else {
        anyhow::bail!("cilium load balancer pool {pool:?} is not an IPv4 CIDR");
    };
    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() != 4 || octets.iter().any(|octet| octet.is_empty()) {
        anyhow::bail!("cilium load balancer pool {pool:?} is not an IPv4 CIDR");
    }
    for octet in octets {
        let n: u16 = octet.parse().map_err(|_| {
            anyhow::anyhow!("cilium load balancer pool {pool:?} is not an IPv4 CIDR")
        })?;
        if n > 255 {
            anyhow::bail!("cilium load balancer pool {pool:?} is not an IPv4 CIDR");
        }
    }
    let bits: u8 = prefix
        .parse()
        .map_err(|_| anyhow::anyhow!("cilium load balancer pool {pool:?} is not an IPv4 CIDR"))?;
    if bits > 32 {
        anyhow::bail!("cilium load balancer pool {pool:?} is not an IPv4 CIDR");
    }
    Ok(())
}

/// Interfaces Cilium should attach and announce on. Empty iface becomes
/// enp6s0, matching the module default. Names are sorted so a rebuild does
/// not reorder the L2 policy.
fn cilium_l2_interfaces(
    nodes: &[crate::kubernetes_orchestrator::ClusterNode],
) -> anyhow::Result<Vec<String>> {
    let mut ifaces = std::collections::BTreeSet::new();
    for node in nodes {
        let iface = if node.iface.is_empty() {
            "enp6s0".to_string()
        } else {
            node.iface.clone()
        };
        validate_cilium_iface(&iface)?;
        ifaces.insert(iface);
    }
    Ok(ifaces.into_iter().collect())
}

/// Cluster-wide values copied into every node's `kcore.kubernetes` block.
#[derive(Debug, Clone)]
pub struct KubernetesRenderSettings {
    pub api_server_endpoint: String,
    pub cluster_dns: String,
    pub ha_enabled: bool,
    pub vip_address: String,
    pub vip_interface: String,
    pub cilium_mode: String,
    pub cilium_mtu: i32,
    pub cilium_tunnel: String,
    pub cilium_auto_direct_routes: bool,
    pub cilium_lb_ip_pools: Vec<String>,
    pub schedule_on_control_plane: bool,
    pub provisioner_manifest: String,
}

/// Generate Kubernetes node configuration for a node in the cluster
pub fn generate_kubernetes_config(
    cluster_name: &str,
    node: &crate::kubernetes_orchestrator::ClusterNode,
    pki: &crate::kubernetes_pki::KubernetesPki,
    cluster_cidr: &str,
    service_cidr: &str,
    etcd_server_cert: &str,
    etcd_server_key: &str,
    etcd_peer_cert: &str,
    etcd_peer_key: &str,
    all_nodes: &[crate::kubernetes_orchestrator::ClusterNode],
    settings: &KubernetesRenderSettings,
) -> anyhow::Result<String> {
    generate_kubernetes_member(
        cluster_name,
        node,
        pki,
        cluster_cidr,
        service_cidr,
        etcd_server_cert,
        etcd_server_key,
        etcd_peer_cert,
        etcd_peer_key,
        all_nodes,
        settings,
        None,
    )
}

pub fn generate_kubernetes_member(
    cluster_name: &str,
    node: &crate::kubernetes_orchestrator::ClusterNode,
    pki: &crate::kubernetes_pki::KubernetesPki,
    cluster_cidr: &str,
    service_cidr: &str,
    etcd_server_cert: &str,
    etcd_server_key: &str,
    etcd_peer_cert: &str,
    etcd_peer_key: &str,
    all_nodes: &[crate::kubernetes_orchestrator::ClusterNode],
    settings: &KubernetesRenderSettings,
    layout: Option<KubernetesInstanceLayout>,
) -> anyhow::Result<String> {
    let peer_port = layout.as_ref().map(|l| l.etcd_peer_port).unwrap_or(2380);
    // Build complete etcd initial cluster string with ALL members
    // For static bootstrap, every node needs the complete list
    // etcd --name is the NixOS hostname (see modules/kcore-kubernetes.nix).
    // The initial-cluster keys must be that same name.
    let etcd_initial_cluster = all_nodes
        .iter()
        .filter(|n| n.role != "worker")
        .map(|n| {
            let name = if n.hostname.is_empty() {
                n.node_id.as_str()
            } else {
                n.hostname.as_str()
            };
            format!("{name}=https://{}:{peer_port}", n.ip_address)
        })
        .collect::<Vec<_>>()
        .join(",");

    let role = if node.role == "worker" {
        "worker"
    } else {
        "control-plane"
    };
    let k8s_config = KubernetesNodeConfig {
        role: role.to_string(),
        cluster_name: cluster_name.to_string(),
        cluster_cidr: cluster_cidr.to_string(),
        service_cidr: service_cidr.to_string(),
        cluster_dns: if settings.cluster_dns.is_empty() {
            "10.96.0.10".to_string()
        } else {
            settings.cluster_dns.clone()
        },
        advertise_address: node.ip_address.clone(),
        iface: if node.iface.is_empty() {
            "enp6s0".to_string()
        } else {
            node.iface.clone()
        },
        api_server_endpoint: api_endpoint_for_nix(&settings.api_server_endpoint, &node.ip_address),
        initial_cluster: node.initial_cluster,
        etcd_initial_cluster,
        cni: "cilium".to_string(),
        cilium_mode: if settings.cilium_mode.is_empty() {
            "native-routing".to_string()
        } else {
            settings.cilium_mode.clone()
        },
        cilium_mtu: settings.cilium_mtu,
        cilium_tunnel: if settings.cilium_tunnel.is_empty() {
            "disabled".to_string()
        } else {
            settings.cilium_tunnel.clone()
        },
        cilium_auto_direct_routes: settings.cilium_auto_direct_routes,
        cilium_lb_ip_pools: settings.cilium_lb_ip_pools.clone(),
        cilium_l2_interfaces: if layout.is_none() {
            for pool in &settings.cilium_lb_ip_pools {
                validate_cilium_pool(pool)?;
            }
            cilium_l2_interfaces(all_nodes)?
        } else {
            Vec::new()
        },
        ha_enabled: settings.ha_enabled,
        vip_address: settings.vip_address.clone(),
        vip_interface: settings.vip_interface.clone(),
        kubeconfig_data: String::new(), // TODO: Include admin kubeconfig
        schedule_on_control_plane: settings.schedule_on_control_plane,
        provisioner_manifest: settings.provisioner_manifest.clone(),
        instance: layout,
    };

    let nix = generate_kubernetes_node_config(&k8s_config);
    // generate_kubernetes_node_config closes `kcore.kubernetes` and then the
    // module. Certificate options have to be inside `kcore.kubernetes`; popping
    // two characters only removed the outer `}`, so the options landed beside
    // the attrset and Nix evaluation failed.
    const MODULE_CLOSE: &str = "  };\n}\n";
    let Some(head) = nix.strip_suffix(MODULE_CLOSE) else {
        anyhow::bail!("kubernetes nix module did not end with the expected close");
    };

    let mut nix = head.to_string();
    nix.push_str("\n    # PKI certificates (written by modules/kcore-kubernetes.nix)\n");
    for (name, value) in [
        ("caCrt", pki.ca_cert.as_str()),
        ("caKey", pki.ca_key.as_str()),
        ("saKey", pki.sa_key.as_str()),
        ("saPub", pki.sa_pub.as_str()),
        ("apiserverCrt", pki.apiserver_cert.as_str()),
        ("apiserverKey", pki.apiserver_key.as_str()),
        (
            "apiserverKubeletClientCrt",
            pki.apiserver_kubelet_client_cert.as_str(),
        ),
        (
            "apiserverKubeletClientKey",
            pki.apiserver_kubelet_client_key.as_str(),
        ),
        (
            "apiserverEtcdClientCrt",
            pki.apiserver_etcd_client_cert.as_str(),
        ),
        (
            "apiserverEtcdClientKey",
            pki.apiserver_etcd_client_key.as_str(),
        ),
        ("etcdCaCrt", pki.etcd_ca_cert.as_str()),
        ("etcdCaKey", pki.etcd_ca_key.as_str()),
        ("etcdServerCrt", etcd_server_cert),
        ("etcdServerKey", etcd_server_key),
        ("etcdPeerCrt", etcd_peer_cert),
        ("etcdPeerKey", etcd_peer_key),
        ("kubeconfigData", pki.admin_kubeconfig.as_str()),
        ("kubeletKubeconfigData", node.kubelet_kubeconfig.as_str()),
    ] {
        nix.push_str(&format!("    {name} = {};\n", nix_multiline(value)));
    }
    nix.push_str(MODULE_CLOSE);

    Ok(nix)
}

/// One cluster's render inputs for a single node.
pub struct KubernetesMemberRender<'a> {
    pub cluster_name: &'a str,
    pub node: &'a crate::kubernetes_orchestrator::ClusterNode,
    pub pki: &'a crate::kubernetes_pki::KubernetesPki,
    pub cluster_cidr: &'a str,
    pub service_cidr: &'a str,
    pub etcd_server_cert: &'a str,
    pub etcd_server_key: &'a str,
    pub etcd_peer_cert: &'a str,
    pub etcd_peer_key: &'a str,
    pub all_nodes: &'a [crate::kubernetes_orchestrator::ClusterNode],
    pub settings: &'a KubernetesRenderSettings,
    pub pod_subnet: String,
}

/// Render every cluster that shares a node into one NixOS module.
///
/// The cluster on API port 6443 stays the primary `kcore.kubernetes` block so
/// its etcd unit, data directory, and Cilium setup do not move. Every other
/// cluster is nested under `additionalClusters`.
pub fn generate_colocated_kubernetes_config(
    members: &[KubernetesMemberRender<'_>],
) -> anyhow::Result<String> {
    if members.is_empty() {
        anyhow::bail!("no kubernetes clusters to render");
    }
    let mut primary: Option<&KubernetesMemberRender<'_>> = None;
    let mut extras: Vec<&KubernetesMemberRender<'_>> = Vec::new();
    for member in members {
        let ports = kubernetes_listen_ports(&member.settings.api_server_endpoint);
        if ports.api_server == 6443 {
            if primary.is_some() {
                anyhow::bail!("two clusters want the primary API port 6443");
            }
            primary = Some(member);
        } else {
            extras.push(member);
        }
    }
    extras.sort_by(|a, b| a.cluster_name.cmp(b.cluster_name));
    if primary.is_some() {
        for member in &extras {
            let ports = kubernetes_listen_ports(&member.settings.api_server_endpoint);
            if ports.offset == 0 || !ports.offset.is_multiple_of(1000) {
                anyhow::bail!(
                    "cluster {} must use an API port at least 1000 above 6443 so its etcd and VRRP ids stay off the primary cluster",
                    member.cluster_name
                );
            }
        }
    }

    let nix = if let Some(member) = primary {
        generate_kubernetes_member(
            member.cluster_name,
            member.node,
            member.pki,
            member.cluster_cidr,
            member.service_cidr,
            member.etcd_server_cert,
            member.etcd_server_key,
            member.etcd_peer_cert,
            member.etcd_peer_key,
            member.all_nodes,
            member.settings,
            None,
        )?
    } else {
        "{\n  kcore.kubernetes = {\n    enable = false;\n  };\n}\n".to_string()
    };

    const MODULE_CLOSE: &str = "  };\n}\n";
    let Some(head) = nix.strip_suffix(MODULE_CLOSE) else {
        anyhow::bail!("kubernetes nix module did not end with the expected close");
    };
    let mut out = head.to_string();
    for member in extras {
        let ports = kubernetes_listen_ports(&member.settings.api_server_endpoint);
        let layout = KubernetesInstanceLayout {
            api_server_port: ports.api_server,
            etcd_client_port: ports.etcd_client,
            etcd_peer_port: ports.etcd_peer,
            kubelet_port: ports.kubelet,
            kubelet_healthz_port: ports.kubelet_healthz,
            controller_manager_port: 10257u16.saturating_add(ports.offset),
            scheduler_port: 10259u16.saturating_add(ports.offset),
            virtual_router_id: ports.virtual_router_id,
            pod_subnet: member.pod_subnet.clone(),
        };
        let full = generate_kubernetes_member(
            member.cluster_name,
            member.node,
            member.pki,
            member.cluster_cidr,
            member.service_cidr,
            member.etcd_server_cert,
            member.etcd_server_key,
            member.etcd_peer_cert,
            member.etcd_peer_key,
            member.all_nodes,
            member.settings,
            Some(layout),
        )?;
        const OPEN: &str = "{\n  kcore.kubernetes = {\n";
        let Some(body) = full
            .strip_prefix(OPEN)
            .and_then(|rest| rest.strip_suffix(MODULE_CLOSE))
        else {
            anyhow::bail!("extra cluster nix did not have the expected shape");
        };
        out.push_str(&format!(
            "    additionalClusters.\"{}\" = {{\n",
            nix_escape(member.cluster_name)
        ));
        out.push_str(body);
        out.push_str("    };\n");
    }
    out.push_str(MODULE_CLOSE);
    Ok(out)
}

/// Listen ports for a cluster. The primary API port 6443 keeps etcd on 2379
/// and 2380. Each extra 1000 on the API port shifts etcd, kubelet, and VRRP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KubernetesListenPorts {
    pub offset: u16,
    pub api_server: u16,
    pub etcd_client: u16,
    pub etcd_peer: u16,
    pub kubelet: u16,
    pub kubelet_healthz: u16,
    pub virtual_router_id: u16,
}

pub fn kubernetes_listen_ports(api_endpoint: &str) -> KubernetesListenPorts {
    let endpoint = api_endpoint
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let api = endpoint
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .unwrap_or(6443);
    let offset = api.saturating_sub(6443);
    let vrid = 40u16.saturating_add(offset / 1000);
    KubernetesListenPorts {
        offset,
        api_server: api,
        etcd_client: 2379u16.saturating_add(offset),
        etcd_peer: 2380u16.saturating_add(offset),
        kubelet: 10250u16.saturating_add(offset),
        kubelet_healthz: 10248u16.saturating_add(offset),
        virtual_router_id: vrid.clamp(1, 255),
    }
}

/// `10.245.0.0/16` and node index 1 becomes `10.245.1.0/24`.
pub fn node_pod_subnet(cluster_cidr: &str, index: usize) -> String {
    let base = cluster_cidr.split('/').next().unwrap_or("");
    let mut octets: Vec<u32> = base
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    if octets.len() != 4 || index > 255 {
        return format!("10.245.{index}.0/24");
    }
    octets[2] = index as u32;
    octets[3] = 0;
    format!("{}.{}.{}.{}/24", octets[0], octets[1], octets[2], octets[3])
}

/// The NixOS module prefixes `https://` itself. Stored endpoints sometimes
/// already include the scheme.
fn api_endpoint_for_nix(raw: &str, fallback_ip: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if trimmed.is_empty() {
        format!("{fallback_ip}:6443")
    } else {
        trimmed.to_string()
    }
}

/// Nix indented string. PEM and kubeconfig text must not close the string or
/// expand as an antiquotation.
fn nix_multiline(value: &str) -> String {
    let mut escaped = value.replace("''", "'''").replace("${", "''${");
    if !escaped.ends_with('\n') {
        escaped.push('\n');
    }
    format!("''{escaped}''")
}
