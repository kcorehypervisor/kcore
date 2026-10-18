#![allow(
    dead_code,
    clippy::too_many_arguments,
    clippy::enum_variant_names,
    clippy::result_large_err
)]

mod apply_summary;
mod client;
mod commands;
mod config;
mod output;
mod path_safety;
mod pki;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn install_crypto_provider() {
    #[cfg(target_os = "macos")]
    let mut provider = rustls::crypto::ring::default_provider();
    #[cfg(not(target_os = "macos"))]
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();

    provider.cipher_suites.retain(|suite| {
        matches!(
            suite.suite(),
            rustls::CipherSuite::TLS13_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS13_AES_128_GCM_SHA256
                | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
                | rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
        )
    });

    provider.kx_groups.retain(|group| {
        matches!(
            group.name(),
            rustls::NamedGroup::secp256r1 | rustls::NamedGroup::secp384r1
        )
    });

    provider
        .install_default()
        .expect("failed to install crypto provider");
}

#[derive(Parser)]
#[command(name = "kctl", version = VERSION, about = "kcore CLI")]
struct Cli {
    /// Path to config file
    #[arg(short = 'c', long, global = true)]
    config: Option<PathBuf>,

    /// Controller address (host:port), repeatable for fallback order
    #[arg(short = 's', long, global = true)]
    controller: Vec<String>,

    /// Skip TLS (connect over plain HTTP)
    #[arg(short = 'k', long, global = true)]
    insecure: bool,

    /// TLS server name (SNI + certificate verification) when the controller address is an IP or the
    /// server certificate does not match the connection host. Overrides `tls-server-name` in config.
    #[arg(long = "tls-server-name", global = true)]
    tls_server_name: Option<String>,

    /// Use per-operator client certificate `~/.kcore/operators/<OPERATOR>/operator.{crt,key}` (overrides `operator:` in config context).
    #[arg(long = "as", global = true, value_name = "OPERATOR")]
    operator_as: Option<String>,

    /// Node address for direct node commands (host:port)
    #[arg(long, global = true)]
    node: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Create a resource
    Create {
        #[command(subcommand)]
        resource: CreateResource,
    },
    /// Delete a resource
    Delete {
        #[command(subcommand)]
        resource: DeleteResource,
    },
    /// Start a resource
    Start {
        #[command(subcommand)]
        resource: StartResource,
    },
    /// Stop a resource
    Stop {
        #[command(subcommand)]
        resource: StopResource,
    },
    /// Attach a resource (e.g. volume to a stopped VM)
    Attach {
        #[command(subcommand)]
        resource: AttachResource,
    },
    /// Detach a resource (e.g. data volume from a stopped VM)
    Detach {
        #[command(subcommand)]
        resource: DetachResource,
    },
    /// Cancel a long-running operation (when phase allows)
    Cancel {
        #[command(subcommand)]
        resource: CancelOpResource,
    },
    /// Set desired resource state (declarative)
    Set {
        #[command(subcommand)]
        resource: SetResource,
    },
    /// Update a resource
    Update {
        #[command(subcommand)]
        resource: UpdateResource,
    },
    /// Set labels on a resource
    Label {
        #[command(subcommand)]
        resource: LabelResource,
    },
    /// Restore a volume from a snapshot (in-place rollback; VM must be stopped)
    Restore {
        #[command(subcommand)]
        resource: RestoreResource,
    },
    /// Flatten a cloned volume (break parent snapshot dependency)
    Flatten {
        #[command(subcommand)]
        resource: FlattenResource,
    },
    /// Grow or shrink a Ceph, LVM, or ZFS volume
    Resize {
        #[command(subcommand)]
        resource: ResizeResource,
    },
    /// Format an LVM or ZFS volume as ext4 or xfs
    Format {
        #[command(subcommand)]
        resource: FormatResource,
    },
    /// Mount an LVM or ZFS volume on its node
    Mount {
        #[command(subcommand)]
        resource: MountResource,
    },
    /// Unmount an LVM or ZFS volume
    Unmount {
        #[command(subcommand)]
        resource: UnmountResource,
    },
    /// Format LUKS on an existing plain RBD volume (data-destructive)
    Encrypt {
        #[command(subcommand)]
        resource: EncryptResource,
    },
    /// Get or list resources
    Get {
        #[command(subcommand)]
        resource: GetResource,
    },
    /// Describe a resource with full details
    Describe {
        #[command(subcommand)]
        resource: DescribeResource,
    },
    /// List GPUs on this machine or across the cluster
    Gpu {
        #[command(subcommand)]
        action: GpuAction,
    },
    /// List PCI devices that can be attached to a VM
    Pci {
        #[command(subcommand)]
        action: PciAction,
    },
    /// Node administration commands
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },
    /// Kubernetes cluster management
    Kubernetes {
        #[command(subcommand)]
        action: KubernetesAction,
    },
    /// Pull a resource
    Pull {
        #[command(subcommand)]
        resource: PullResource,
    },
    /// Manage SSH keys
    SshKey {
        #[command(subcommand)]
        action: SshKeyAction,
    },
    /// Manage RBAC operators (per-operator client certificates)
    Operator {
        #[command(subcommand)]
        action: OperatorAction,
    },
    /// Write a consistent SQLite snapshot of the controller database
    Backup {
        /// File to write
        #[arg(short = 'o', long)]
        output: PathBuf,
    },
    /// Export a volume snapshot to a sparse raw file
    Export {
        #[command(subcommand)]
        resource: ExportResource,
    },
    /// Import a sparse raw file into a new (or existing) volume
    Import {
        #[command(subcommand)]
        resource: ImportResource,
    },
    /// Drain a node (migrate all VMs to other nodes)
    Drain {
        #[command(subcommand)]
        resource: DrainResource,
    },
    /// Migrate a VM. GPU VMs move cold onto a free or named GPU.
    Migrate {
        #[command(subcommand)]
        resource: MigrateResource,
    },
    /// Rotate certificates
    Rotate {
        #[command(subcommand)]
        resource: RotateResource,
    },
    /// Revoke a certificate (adds it to the cluster CRL and OCSP responder)
    Revoke {
        #[command(subcommand)]
        resource: RevokeResource,
    },
    /// Apply a NixOS configuration to the controller
    Apply {
        /// Path to the NixOS configuration file
        #[arg(short = 'f', long = "filename")]
        file: String,
        /// Print the config without applying
        #[arg(long)]
        dry_run: bool,
    },
    /// Show a controller-side pre-flight diff/safety report for a manifest.
    /// Currently supports `kind: DiskLayout` manifests (`spec.diskLayout` YAML,
    /// or `layoutNix` / `layoutNixFile`); other kinds will be added as their
    /// classifiers land.
    Diff {
        /// Path to the manifest file
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Show version
    Version,
    /// Attach to a VM serial console (Ctrl-] to detach)
    Console {
        /// VM name or id
        name: String,
    },
    /// Unified workload operations via controller API
    Workload {
        #[command(subcommand)]
        action: WorkloadAction,
    },
    /// Manage security groups
    #[command(name = "security-group", alias = "sg")]
    SecurityGroup {
        #[command(subcommand)]
        action: SecurityGroupAction,
    },
}

#[derive(Subcommand)]
enum NodeLvmAction {
    /// Create a logical volume in the node's volume group
    Create {
        /// Volume id. Used as the LV name when --lv-name is omitted, with the kcore- prefix.
        #[arg(long)]
        id: String,
        /// Size in bytes
        #[arg(long)]
        size_bytes: i64,
        /// Logical volume name without the VM prefix
        #[arg(long)]
        lv_name: Option<String>,
    },
    /// Grow or shrink a logical volume. Shrink needs --allow-shrink and an unmounted ext4 filesystem.
    Resize {
        /// Device path, /dev/<vg>/<lv>
        #[arg(long)]
        device: String,
        /// New size in bytes, a multiple of 1 MiB
        #[arg(long)]
        size_bytes: i64,
        /// Allow the volume to become smaller
        #[arg(long)]
        allow_shrink: bool,
    },
    /// Format a logical volume as ext4 or xfs
    Format {
        /// Device path, /dev/<vg>/<lv>
        #[arg(long)]
        device: String,
        /// ext4 or xfs
        #[arg(long)]
        fs: String,
        /// Replace an existing filesystem
        #[arg(long)]
        force: bool,
    },
    /// Mount a logical volume under /var/lib/kcore or /mnt/kcore
    Mount {
        /// Device path, /dev/<vg>/<lv>
        #[arg(long)]
        device: String,
        /// Mount point
        #[arg(long)]
        path: String,
        /// ext4 or xfs. Empty uses the filesystem already on the volume.
        #[arg(long)]
        fs: Option<String>,
        /// Install a systemd mount unit so the volume returns after reboot
        #[arg(long)]
        persistent: bool,
    },
    /// Unmount a logical volume
    Unmount {
        /// Mount point
        #[arg(long)]
        path: String,
        /// Also remove the systemd mount unit
        #[arg(long)]
        persistent: bool,
    },
}

#[derive(Subcommand)]
enum NodeZfsAction {
    /// Create a zvol in the node's pool
    Create {
        /// Volume id. Used as the zvol name when --dataset is omitted, with the kcore- prefix.
        #[arg(long)]
        id: String,
        /// Size in bytes
        #[arg(long)]
        size_bytes: i64,
        /// Zvol name without the VM prefix
        #[arg(long)]
        dataset: Option<String>,
    },
    /// Grow or shrink a zvol. Shrink needs --allow-shrink and an unmounted ext4 filesystem.
    Resize {
        /// Device path, /dev/zvol/<pool>/<name>
        #[arg(long)]
        device: String,
        /// New size in bytes, a multiple of 1 MiB
        #[arg(long)]
        size_bytes: i64,
        /// Allow the volume to become smaller
        #[arg(long)]
        allow_shrink: bool,
    },
    /// Format a zvol as ext4 or xfs
    Format {
        /// Device path, /dev/zvol/<pool>/<name>
        #[arg(long)]
        device: String,
        /// ext4 or xfs
        #[arg(long)]
        fs: String,
        /// Replace an existing filesystem
        #[arg(long)]
        force: bool,
    },
    /// Mount a zvol under /var/lib/kcore or /mnt/kcore
    Mount {
        /// Device path, /dev/zvol/<pool>/<name>
        #[arg(long)]
        device: String,
        /// Mount point
        #[arg(long)]
        path: String,
        /// ext4 or xfs. Empty uses the filesystem already on the volume.
        #[arg(long)]
        fs: Option<String>,
        /// Install a systemd mount unit so the volume returns after reboot
        #[arg(long)]
        persistent: bool,
    },
    /// Unmount a zvol
    Unmount {
        /// Mount point
        #[arg(long)]
        path: String,
        /// Also remove the systemd mount unit
        #[arg(long)]
        persistent: bool,
    },
}

#[derive(Subcommand)]
enum UpdateClusterCmd {
    /// Dry-run: resolve nodes and compatibility (reads YAML manifest)
    Plan {
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Create or update a cluster update from YAML manifest
    Apply {
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Show one cluster update and per-node status
    Get { name: String },
    /// List cluster updates
    List,
    /// Approve a manual cluster update
    Approve { name: String },
    /// Cancel a cluster update
    Cancel { name: String },
    /// Request rollback on nodes (best-effort)
    Rollback { name: String },
    /// Boot a generation that was staged because the kernel or guest units changed.
    Reboot { name: String, node: String },
}

#[derive(Subcommand)]
enum RotateResource {
    /// Rotate controller TLS certificate with a new address SAN
    Certs {
        /// New controller address (host:port) for the certificate SAN
        #[arg(long)]
        controller: String,
        /// Certificate directory (defaults to active context's cert dir)
        #[arg(long)]
        certs_dir: Option<PathBuf>,
    },
    /// Generate a new sub-CA and push it to the controller
    #[command(name = "sub-ca")]
    SubCa {
        /// Certificate directory (defaults to active context's cert dir)
        #[arg(long)]
        certs_dir: Option<PathBuf>,
    },
    /// Rotate node certificates via CSR (private keys stay on the nodes)
    #[command(name = "node-certs", alias = "node-cert")]
    NodeCerts {
        /// Node to rotate
        #[arg(long)]
        node: Option<String>,
        /// Rotate every approved node
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum RevokeResource {
    /// Revoke an issued certificate by serial, subject CN or node id
    #[command(name = "cert", alias = "certificate")]
    Cert {
        /// Serial number in hex (as shown by `kctl get certificates`)
        #[arg(long)]
        serial: Option<String>,
        /// Subject common name, e.g. kctl:alice
        #[arg(long)]
        subject: Option<String>,
        /// Node id whose active certificate should be revoked
        #[arg(long)]
        node: Option<String>,
        /// RFC 5280 reason: unspecified, key-compromise, ca-compromise,
        /// affiliation-changed, superseded, cessation-of-operation,
        /// certificate-hold, remove-from-crl, privilege-withdrawn,
        /// aa-compromise
        #[arg(long, default_value = "unspecified")]
        reason: String,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum CreateResource {
    /// Create a virtual machine
    Vm {
        /// VM name (optional if using -f)
        name: Option<String>,
        /// Create from YAML manifest
        #[arg(short = 'f', long = "filename")]
        file: Option<String>,
        /// Number of CPU cores
        #[arg(long, default_value = "2")]
        cpu: i32,
        /// Memory size (e.g. 2G, 4096M)
        #[arg(long, default_value = "2G")]
        memory: String,
        /// VM boot image HTTPS URL
        #[arg(long)]
        image: Option<String>,
        /// Required SHA256 for --image URL
        #[arg(long = "image-sha256")]
        image_sha256: Option<String>,
        /// Node-local image path (alternative to --image URL mode)
        #[arg(long = "image-path")]
        image_path: Option<String>,
        /// Image format for --image-path mode (raw or qcow2)
        #[arg(long = "image-format")]
        image_format: Option<String>,
        /// Network name
        #[arg(long)]
        network: Option<String>,
        /// Additional NIC on another network. Repeat for each extra interface.
        #[arg(long = "extra-network")]
        extra_networks: Vec<String>,
        /// Target node (optional, controller picks if empty)
        #[arg(long = "target-node")]
        target_node: Option<String>,
        /// Wait until VM reaches running state
        #[arg(long)]
        wait: bool,
        /// Wait until VM is running and SSH port is reachable from node host
        #[arg(long = "wait-for-ssh")]
        wait_for_ssh: bool,
        /// Max wait time in seconds for --wait/--wait-for-ssh
        #[arg(long = "wait-timeout-seconds", default_value_t = 300)]
        wait_timeout_seconds: u64,
        /// SSH port to probe when using --wait-for-ssh
        #[arg(long = "ssh-port", default_value_t = 22)]
        ssh_port: i32,
        /// SSH TCP probe timeout (milliseconds) when using --wait-for-ssh
        #[arg(long = "ssh-probe-timeout-ms", default_value_t = 1500)]
        ssh_probe_timeout_ms: i32,
        /// SSH key names to inject (can specify multiple times)
        #[arg(long = "ssh-key")]
        ssh_keys: Vec<String>,
        /// SSH public key values to embed directly in generated cloud-init user-data
        #[arg(long = "ssh-public-key")]
        ssh_public_keys: Vec<String>,
        /// Path to cloud-init user-data YAML (overrides default/generated user-data)
        #[arg(long = "cloud-init-user-data-file")]
        cloud_init_user_data_file: Option<String>,
        /// VM login username for generated cloud-init user-data
        #[arg(long)]
        username: Option<String>,
        /// VM login password for generated cloud-init user-data (non-compliant unless --compliant=false)
        #[arg(long)]
        password: Option<String>,
        /// Compliance mode (default true). Set false to acknowledge non-compliant auth like password login.
        #[arg(long = "compliant", default_value_t = true, action = clap::ArgAction::Set)]
        compliant: bool,
        /// VM storage backend (filesystem, lvm, zfs). Can also be set in YAML manifest under spec.storageBackend.
        #[arg(long = "storage-backend", value_enum)]
        storage_backend: Option<StorageBackend>,
        /// VM storage size in bytes. Can also be set in YAML manifest under spec.storageSizeBytes.
        #[arg(long = "storage-size-bytes")]
        storage_size_bytes: Option<i64>,
        /// Target datacenter (optional; controller picks any DC if empty)
        #[arg(long = "dc")]
        target_dc: Option<String>,
        /// AI GPU to assign, by inventory name or family (repeatable). Requires --target-node.
        /// Example: --gpu radeon0. A family (`radeon`, `nvidia`, `intel`, `accel`) takes the first free one.
        #[arg(long = "gpu")]
        gpu: Vec<String>,
        /// NVIDIA vGPU slice (repeatable). Requires --target-node.
        /// Example: --vgpu A2-4C, --vgpu nvidia0=A2-4C, or --vgpu nvidia0/3=A2-4C.
        #[arg(long = "vgpu")]
        vgpu: Vec<String>,
        /// Ethernet NIC to assign, by inventory name or family (repeatable).
        /// Example: --nic nic0. Family `nic` takes the first free one.
        #[arg(long = "nic")]
        nic: Vec<String>,
        /// NVMe controller to assign, by inventory name or family (repeatable).
        /// Example: --nvme nvme0. Family `nvme` takes the first free one.
        #[arg(long = "nvme")]
        nvme: Vec<String>,
        /// Raw PCI address to pass through (repeatable). Every function in the IOMMU group.
        /// Example: --pci 0000:03:00.0 --pci 0000:03:00.1
        #[arg(long = "pci")]
        pci: Vec<String>,
        /// Required node label (key=value). Repeat to require every label.
        #[arg(long = "label")]
        node_labels: Vec<String>,
        /// VM label key=value. Repeat for more than one. Example: kcore-vault/restore=job
        #[arg(long = "vm-label")]
        vm_label: Vec<String>,
        /// Keep this VM off nodes that already host the same anti-affinity group
        #[arg(long = "anti-affinity")]
        anti_affinity: Option<String>,
    },
    /// Create a container on a node
    Container {
        /// Container name
        name: String,
        /// OCI image reference (e.g. nginx:alpine)
        #[arg(long)]
        image: String,
        /// Runtime network name (optional)
        #[arg(long)]
        network: Option<String>,
        /// Port mapping (repeatable), e.g. 8080:80
        #[arg(long = "port")]
        ports: Vec<String>,
        /// Env var (repeatable), e.g. KEY=VALUE
        #[arg(long = "env")]
        env: Vec<String>,
        /// Command arguments appended after image
        #[arg(long = "cmd")]
        cmd: Vec<String>,
    },
    /// Create a network on a node (declarative)
    Network {
        /// Network name
        name: String,
        /// External IP used as NAT source for this network
        #[arg(long = "external-ip")]
        external_ip: String,
        /// Gateway IP assigned to the bridge
        #[arg(long = "gateway-ip")]
        gateway_ip: String,
        /// Internal netmask (default: 255.255.255.0)
        #[arg(long = "internal-netmask", default_value = "255.255.255.0")]
        internal_netmask: String,
        /// Target node (optional, controller picks if empty)
        #[arg(long = "target-node")]
        target_node: Option<String>,
        /// 802.1Q VLAN tag (0 = no VLAN)
        #[arg(long = "vlan-id", default_value_t = 0)]
        vlan_id: i32,
        /// Network type: nat, bridge, or vxlan (default: nat)
        #[arg(long = "type", default_value = "nat")]
        network_type: String,
        /// Disable outbound NAT (for vxlan networks; makes overlay fully isolated)
        #[arg(long = "no-outbound-nat")]
        no_outbound_nat: bool,
        /// Optional IPv6 /64. The gateway must be the first address of this prefix.
        #[arg(long = "ipv6-prefix")]
        ipv6_prefix: Option<String>,
        /// IPv6 gateway on the bridge (prefix::1)
        #[arg(long = "ipv6-gateway")]
        ipv6_gateway: Option<String>,
        /// Drop VM-to-VM traffic on this bridge except security-group allows, the gateway, and DHCP
        #[arg(long = "east-west-firewall")]
        east_west_firewall: bool,
    },
    /// Create a volume. Ceph by default; lvm and zfs need --node.
    Volume {
        /// Volume name
        name: String,
        /// Size in bytes
        #[arg(long = "size-bytes", default_value_t = 0)]
        size_bytes: i64,
        /// ceph, lvm, or zfs
        #[arg(long, default_value = "ceph")]
        storage_class: String,
        /// Node id. Required for lvm and zfs.
        #[arg(long)]
        node: Option<String>,
        /// Logical volume or zvol name. Empty keeps the kcore- prefix.
        #[arg(long)]
        volume_name: Option<String>,
        /// Optional VM id or name to attach immediately (Ceph only; VM must be stopped)
        #[arg(long)]
        vm: Option<String>,
        /// Clone from an existing volume snapshot (Ceph only)
        #[arg(long = "from-snapshot")]
        from_snapshot: Option<String>,
        /// Host-side LUKS over RBD (E2); DEK wrapped by cluster master key
        #[arg(long)]
        encrypt: bool,
    },
    /// Create a crash-consistent snapshot of a volume
    VolumeSnapshot {
        /// Volume id or name
        volume: String,
        /// Snapshot name (optional; auto-generated if empty)
        #[arg(long)]
        name: Option<String>,
    },
    /// Create a SnapshotPolicy (scheduled crash-consistent snaps + retention)
    SnapshotPolicy {
        name: String,
        /// Select volumes by VM id or name
        #[arg(long)]
        vm: Option<String>,
        /// Select a single volume by id or name
        #[arg(long)]
        volume: Option<String>,
        /// `@hourly`, `@daily`, or `every:<seconds>`
        #[arg(long, default_value = "@daily")]
        schedule: String,
        /// Number of snapshots to retain per volume
        #[arg(long, default_value_t = 7)]
        keep: i32,
        /// Disable the policy on create/update
        #[arg(long)]
        disabled: bool,
    },
    /// Create a SharedFilesystem (CephFS) from a YAML manifest
    #[command(name = "shared-filesystem", alias = "sharedfilesystem")]
    SharedFilesystem {
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Create an ObjectStore (RGW) from a YAML manifest
    #[command(name = "object-store", alias = "objectstore")]
    ObjectStore {
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Create one PostgreSQL database from the NixOS postgresql package
    #[command(alias = "postgres")]
    Postgresql {
        /// Instance name (optional when using -f)
        name: Option<String>,
        /// Create from a Postgresql YAML manifest
        #[arg(short = 'f', long = "filename")]
        file: Option<String>,
        /// Database name. Defaults to the instance name.
        #[arg(long)]
        database: Option<String>,
        /// nixpkgs attribute: postgresql, postgresql_14, postgresql_15, postgresql_16, postgresql_17
        #[arg(long, default_value = "postgresql")]
        package: String,
        /// TCP port recorded for the service. Clients still use the local Unix socket in v1.
        #[arg(long, default_value_t = 5432)]
        port: i32,
        /// Target node (optional, controller picks a free ready node)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Create cluster PKI and local context for mTLS
    Cluster {
        /// Controller address (host:port)
        #[arg(long)]
        controller: String,
        /// Optional cert output directory
        #[arg(long)]
        certs_dir: Option<PathBuf>,
        /// Context name to write in config
        #[arg(long, default_value = "default")]
        context: String,
        /// Overwrite existing certificate files
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum DeleteResource {
    /// Delete a virtual machine
    Vm {
        /// VM ID or name
        vm_id: String,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
        /// Also delete Ceph data volumes belonging to the VM
        #[arg(long = "delete-data-volumes")]
        delete_data_volumes: bool,
    },
    /// Delete a detached data volume
    Volume { name: String },
    /// Delete a volume snapshot
    VolumeSnapshot { name: String },
    /// Delete a SnapshotPolicy
    SnapshotPolicy { name: String },
    /// Delete a network
    Network {
        /// Network name
        name: String,
        /// Target node (optional; required if network exists on multiple nodes)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Delete a PostgreSQL database from the node config
    #[command(alias = "postgres")]
    Postgresql {
        /// Instance name
        name: String,
    },
    /// Delete an image from a node
    Image {
        /// Image name
        name: String,
        /// Force deletion
        #[arg(long)]
        force: bool,
    },
    /// Delete a container from a node
    Container {
        /// Container name
        name: String,
        /// Force deletion
        #[arg(long)]
        force: bool,
    },
    /// Delete a DiskLayout (controller-side; does not touch the node)
    #[command(name = "disk-layout", alias = "disklayout")]
    DiskLayout {
        /// DiskLayout name
        name: String,
    },
    /// Delete a CephCluster. Stops mons and OSDs on members first.
    /// `--force` deletes the record when a member cannot be reached.
    #[command(name = "ceph-cluster", alias = "cephcluster")]
    CephCluster {
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Delete a SharedFilesystem (CephFS) resource
    #[command(name = "shared-filesystem", alias = "sharedfilesystem")]
    SharedFilesystem { name: String },
    /// Delete an ObjectStore (RGW) resource
    #[command(name = "object-store", alias = "objectstore")]
    ObjectStore { name: String },
}

#[derive(Subcommand)]
enum StartResource {
    /// Request desired running state for a virtual machine (declarative apply)
    Vm {
        /// VM ID or name
        vm_id: String,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Start a container on a node
    Container {
        /// Container name
        name: String,
    },
}

#[derive(Subcommand)]
enum StopResource {
    /// Request desired stopped state for a virtual machine (declarative apply)
    Vm {
        /// VM ID or name
        vm_id: String,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Stop a container on a node
    Container {
        /// Container name
        name: String,
    },
}

#[derive(Subcommand)]
enum AttachResource {
    /// Attach a detached data volume to a stopped Ceph VM
    Volume {
        name: String,
        #[arg(long)]
        vm: String,
    },
}

#[derive(Subcommand)]
enum DetachResource {
    /// Detach a data volume from its VM (VM must be stopped)
    Volume { name: String },
}

#[derive(Subcommand)]
enum CancelOpResource {
    /// Cancel a VM operation (only while still Preparing for live migrate)
    Operation { id: String },
}

#[derive(Subcommand)]
enum RestoreResource {
    Volume {
        name: String,
        #[arg(long)]
        snapshot: String,
    },
    /// Replace the controller database from a snapshot taken by `kctl backup`
    Controller {
        /// Snapshot file
        #[arg(short = 'f', long)]
        file: PathBuf,
        /// Required. Restore overwrites the live controller database.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum FlattenResource {
    Volume { name: String },
}

#[derive(Subcommand)]
enum ResizeResource {
    /// Grow or shrink a volume. LVM and ZFS shrink an unmounted ext4 filesystem.
    Volume {
        name: String,
        #[arg(long = "size-bytes")]
        size_bytes: i64,
        /// Attempt guest filesystem grow when SSH is available (best-effort)
        #[arg(long)]
        grow_filesystem: bool,
        /// Shrink via `rbd resize --allow-shrink` (destroys tail; volume must be detached)
        #[arg(long = "allow-shrink")]
        allow_shrink: bool,
    },
}

#[derive(Subcommand)]
enum FormatResource {
    /// Format an LVM or ZFS volume
    Volume {
        name: String,
        /// ext4 or xfs
        #[arg(long)]
        fs: String,
        /// Replace an existing filesystem
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum MountResource {
    /// Mount an LVM or ZFS volume under /var/lib/kcore or /mnt/kcore
    Volume {
        name: String,
        #[arg(long)]
        path: String,
        /// ext4 or xfs. Empty uses the filesystem already on the volume.
        #[arg(long)]
        fs: Option<String>,
        /// Install a systemd mount unit
        #[arg(long)]
        persistent: bool,
    },
}

#[derive(Subcommand)]
enum UnmountResource {
    /// Unmount an LVM or ZFS volume
    Volume {
        name: String,
        /// Also remove the systemd mount unit
        #[arg(long)]
        persistent: bool,
    },
}

#[derive(Subcommand)]
enum EncryptResource {
    /// LUKS-format a plain volume (destroys existing plaintext; VM stopped or detached)
    Volume {
        name: String,
        /// Acknowledge that all existing volume data will be destroyed (no offline migration)
        #[arg(long = "destroy-data")]
        destroy_data: bool,
    },
}

#[derive(Subcommand)]
enum ExportResource {
    /// Export a volume snapshot to a sparse raw image
    Volume {
        /// Volume id or name
        name: String,
        /// Snapshot name (VolumeSnapshotInfo.name)
        #[arg(long)]
        snapshot: String,
        /// Optional older snapshot; only changed extents are written
        #[arg(long = "from-snapshot")]
        from_snapshot: Option<String>,
        /// Output path for the sparse raw file
        #[arg(short = 'o', long)]
        output: PathBuf,
        /// Transfer block size in bytes (default 4 MiB)
        #[arg(long = "block-size", default_value_t = 4 * 1024 * 1024)]
        block_size: i32,
    },
}

#[derive(Subcommand)]
enum ImportResource {
    /// Import a sparse raw image into a volume
    Volume {
        /// Input sparse raw file
        #[arg(short = 'f', long)]
        file: PathBuf,
        /// New volume name (required unless --base is set)
        #[arg(long)]
        name: Option<String>,
        /// Existing detached volume to overwrite instead of creating
        #[arg(long)]
        base: Option<String>,
        /// Size in bytes for a new volume (default: file logical size)
        #[arg(long = "size-bytes")]
        size_bytes: Option<i64>,
        /// ceph, lvm, or zfs
        #[arg(long, default_value = "ceph")]
        storage_class: String,
        /// Node id (required for lvm/zfs)
        #[arg(long)]
        node: Option<String>,
        /// Punch holes for zero runs where the backend supports it
        #[arg(long, default_value_t = true)]
        punch_zeros: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum DesiredVmState {
    Running,
    Stopped,
}

#[derive(Subcommand)]
enum SetResource {
    /// Set desired state for a virtual machine (declarative apply)
    Vm {
        /// VM ID or name
        vm_id: String,
        /// Desired VM state
        #[arg(long, value_enum)]
        state: DesiredVmState,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Store Ceph mgr Dashboard login credentials (cluster-admin)
    #[command(name = "ceph-dashboard-credentials", alias = "ceph-dashboard-creds")]
    CephDashboardCredentials {
        /// Logical CephCluster name these credentials belong to
        name: String,
        /// Dashboard base URL (e.g. https://192.168.40.103:8444/)
        #[arg(long)]
        url: String,
        /// Dashboard username
        #[arg(long, default_value = "admin")]
        username: String,
        /// Dashboard password (Ceph stores only a hash; kcore keeps the plaintext)
        #[arg(long)]
        password: String,
    },
}

#[derive(Subcommand)]
enum LabelResource {
    /// Merge key=value labels onto a virtual machine
    Vm {
        /// VM ID or name
        vm_id: String,
        /// Label key=value. Repeat to set more than one.
        #[arg(long = "label", required = true)]
        label: Vec<String>,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
}

#[derive(Subcommand)]
enum UpdateResource {
    /// Update a virtual machine (resize CPU/memory)
    Vm {
        /// VM ID or name
        vm_id: String,
        /// New CPU count
        #[arg(long)]
        cpu: Option<i32>,
        /// New memory size (e.g. 4G, 8192M)
        #[arg(long)]
        memory: Option<String>,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Cluster / host OS updates (`ClusterUpdate` resource)
    #[command(name = "cluster", alias = "os")]
    Cluster {
        #[command(subcommand)]
        action: UpdateClusterCmd,
    },
}

#[derive(Subcommand)]
enum GetResource {
    /// Get or list virtual machines
    #[command(alias = "vm")]
    Vms {
        /// VM ID or name (omit to list all)
        name: Option<String>,
        /// Filter by node
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Get or list containers on a node
    #[command(alias = "container")]
    Containers {
        /// Container name (omit to list all)
        name: Option<String>,
    },
    /// Get or list nodes
    #[command(alias = "node")]
    Nodes {
        /// Node ID (omit to list all)
        name: Option<String>,
    },
    /// List custom networks
    #[command(alias = "network")]
    Networks {
        /// Filter by node
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Get or list PostgreSQL databases
    #[command(name = "postgresql", alias = "postgres")]
    Postgresql {
        /// Instance name (omit to list all)
        name: Option<String>,
        /// Filter by node
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// List storage classes
    #[command(name = "storage-class", alias = "storage-classes")]
    StorageClass,
    /// List VM volumes across the cluster
    Volumes {
        /// Optional VM id or name filter
        #[arg(long)]
        vm: Option<String>,
    },
    /// Get one volume by name or id
    Volume { name: String },
    /// List long-running VM operations (live migrate, …)
    Operations {
        #[arg(long)]
        include_finished: bool,
        #[arg(long)]
        vm: Option<String>,
    },
    /// Get one VM operation by id
    Operation { id: String },
    /// List volume snapshots
    #[command(alias = "snapshot")]
    Snapshots {
        #[arg(long)]
        volume: Option<String>,
    },
    /// List or get SnapshotPolicy resources
    #[command(alias = "snapshot-policy")]
    SnapshotPolicies {
        /// Policy name (omit to list all)
        name: Option<String>,
    },
    /// Show compliance report
    #[command(alias = "compliance")]
    ComplianceReport,
    /// Node readiness, VM placement, certificate expiry, and replication conflicts
    #[command(name = "cluster-health")]
    ClusterHealth,
    /// Show the controller cryptographic configuration
    #[command(name = "crypto-config", alias = "crypto")]
    CryptoConfig,
    /// Export a CycloneDX SBOM from the controller
    Sbom {
        /// `crates` (default) or `iso-closure`
        #[arg(long, default_value = "crates")]
        kind: String,
        /// Write the document to this path. Without it, only the summary is printed.
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
    /// List recent audit events
    #[command(name = "audit-events", alias = "audit")]
    AuditEvents {
        /// Max events to return (default 100, max 1000)
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Filter by exact action name (e.g. CreateVm)
        #[arg(long)]
        action: Option<String>,
        /// Only events at or after this RFC3339 timestamp
        #[arg(long)]
        since: Option<String>,
    },
    /// List unresolved replication conflicts
    Conflicts {
        /// Max conflicts to return
        #[arg(long, default_value_t = 100)]
        limit: i32,
    },
    /// Show replication zero-manual SLO status
    #[command(name = "replication-status")]
    ReplicationStatus {
        /// Exit non-zero when status is unhealthy
        #[arg(long)]
        require_healthy: bool,
    },
    /// List DiskLayout resources
    #[command(name = "disk-layouts", alias = "disk-layout", alias = "disklayout")]
    DiskLayouts {
        /// Optional node filter (only show layouts targeted at this node)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// List CephCluster resources
    #[command(name = "ceph-clusters", alias = "ceph-cluster", alias = "cephcluster")]
    CephClusters,
    /// Get Ceph mgr Dashboard login credentials (cluster-admin)
    #[command(name = "ceph-dashboard-credentials", alias = "ceph-dashboard-creds")]
    CephDashboardCredentials {
        /// Logical CephCluster name
        name: String,
    },
    /// List SharedFilesystem resources
    #[command(
        name = "shared-filesystems",
        alias = "shared-filesystem",
        alias = "sharedfilesystem"
    )]
    SharedFilesystems,
    /// List ObjectStore resources
    #[command(name = "object-stores", alias = "object-store", alias = "objectstore")]
    ObjectStores,
    /// List the controller's certificate inventory
    #[command(name = "certificates", alias = "certificate", alias = "certs")]
    Certificates {
        /// Only certificates issued to this node
        #[arg(long)]
        node: Option<String>,
        /// Filter by status: active, rotated, revoked or all
        #[arg(long)]
        status: Option<String>,
        /// Only certificates expiring within this many days
        #[arg(long = "expiring-within-days", default_value_t = 0)]
        expiring_within_days: i32,
    },
    /// Show certificate rotation and revocation health
    #[command(name = "pki-status", alias = "pki")]
    PkiStatus,
    /// Fetch the current certificate revocation list
    Crl {
        /// Write the CRL to this file (.der for DER, anything else for PEM)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// Show live-migrate receive session state for a VM (read-only)
    #[command(name = "migrate-session", alias = "migrate-sessions")]
    MigrateSession {
        /// VM id or name
        vm_id: String,
        /// Only query this node (default: every CephCluster member)
        #[arg(long)]
        node: Option<String>,
    },
}

#[derive(Subcommand)]
enum DescribeResource {
    /// Describe a virtual machine
    #[command(alias = "vms")]
    Vm {
        /// VM ID or name
        name: String,
        /// Target node (optional)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Describe a node
    #[command(alias = "nodes")]
    Node {
        /// Node ID
        name: String,
    },
    /// Describe a network
    #[command(alias = "networks")]
    Network {
        /// Network name
        name: String,
        /// Target node (optional; required if name exists on multiple nodes)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Describe a PostgreSQL database
    #[command(name = "postgresql", alias = "postgres")]
    Postgresql {
        /// Instance name
        name: String,
    },
    /// Describe a storage class
    #[command(name = "storage-class", alias = "storage-classes")]
    StorageClass {
        /// Storage class name (filesystem, lvm, zfs, unspecified)
        name: String,
    },
    /// Describe an SSH key
    #[command(name = "ssh-key", alias = "sshkey")]
    SshKey {
        /// Key name
        name: String,
    },
    /// Describe cluster compliance report
    #[command(name = "compliance-report", alias = "compliance")]
    ComplianceReport,
    /// Describe the controller cryptographic configuration
    #[command(name = "crypto-config", alias = "crypto")]
    CryptoConfig,
    /// Describe a DiskLayout (current generation, observed status, body)
    #[command(name = "disk-layout", alias = "disklayout")]
    DiskLayout {
        /// DiskLayout name
        name: String,
    },
    /// Describe a CephCluster
    #[command(name = "ceph-cluster", alias = "cephcluster")]
    CephCluster { name: String },
    /// Describe a SharedFilesystem
    #[command(name = "shared-filesystem", alias = "sharedfilesystem")]
    SharedFilesystem { name: String },
    /// Describe an ObjectStore
    #[command(name = "object-store", alias = "objectstore")]
    ObjectStore { name: String },
}

#[derive(Subcommand)]
enum NodeAction {
    /// List block devices on a node
    Disks,
    /// List network interfaces on a node
    Nics,
    /// Create, resize, format, and mount a logical volume on this node
    Lvm {
        #[command(subcommand)]
        action: NodeLvmAction,
    },
    /// Create, resize, format, and mount a zvol on this node
    Zfs {
        #[command(subcommand)]
        action: NodeZfsAction,
    },
    /// Install kcore to disk on a node
    Install {
        /// OS disk (e.g. /dev/sda)
        #[arg(long)]
        os_disk: String,
        /// Data disks (optional; omit for single-disk installs where OS and VMs share one disk)
        #[arg(long)]
        data_disk: Vec<String>,
        /// Controller to join after install
        #[arg(long)]
        join_controller: Vec<String>,
        /// Install and run controller on this node
        #[arg(long)]
        run_controller: bool,
        /// Storage mode for data disks: filesystem, lvm, or zfs
        #[arg(long, default_value = "filesystem")]
        data_disk_mode: String,
        /// Storage backend (filesystem, lvm, zfs). Defaults to filesystem for single-disk installs
        #[arg(long = "storage-backend", value_enum)]
        storage_backend: Option<StorageBackend>,
        /// Optional LVM VG name (used when backend is lvm)
        #[arg(long = "lvm-vg-name")]
        lvm_vg_name: Option<String>,
        /// Optional LVM LV prefix (used when backend is lvm)
        #[arg(long = "lvm-lv-prefix")]
        lvm_lv_prefix: Option<String>,
        /// Optional ZFS pool name (used when backend is zfs)
        #[arg(long = "zfs-pool-name")]
        zfs_pool_name: Option<String>,
        /// Optional ZFS dataset prefix (used when backend is zfs)
        #[arg(long = "zfs-dataset-prefix")]
        zfs_dataset_prefix: Option<String>,
        /// Disable VXLAN overlay networking on this node
        #[arg(long = "disable-vxlan")]
        disable_vxlan: bool,
        /// Datacenter identity for the installed node (default: DC1)
        #[arg(long = "dc-id", default_value = "DC1")]
        dc_id: String,
        /// Hostname for the installed node (auto-generated from IP if not set)
        #[arg(long)]
        hostname: Option<String>,
        /// Node identity for registration (defaults to hostname if not set)
        #[arg(long = "node-id")]
        node_id: Option<String>,
    },
    /// Approve a pending node to join the cluster
    Approve {
        /// Node ID to approve
        node_id: String,
    },
    /// Reject a pending node
    Reject {
        /// Node ID to reject
        node_id: String,
    },
    /// Stop scheduling new VMs onto a node without moving the ones already there
    Cordon {
        /// Node ID to cordon
        node_id: String,
    },
    /// Make a cordoned node schedulable again
    Uncordon {
        /// Node ID to uncordon
        node_id: String,
    },
    /// Remove a node that has no VMs or networks, and revoke its certificates
    Delete {
        /// Node ID to delete
        node_id: String,
    },
    /// Apply a NixOS configuration to a node
    ApplyNix {
        /// Path to the NixOS configuration file
        #[arg(short = 'f', long = "filename")]
        file: String,
        /// Write config without running nixos-rebuild
        #[arg(long)]
        no_rebuild: bool,
    },
    /// Validate/apply a day-2 disk layout on a node (built on disko)
    #[command(visible_alias = "apply-disko")]
    ApplyDisk {
        /// Path to the disk-layout Nix expression file
        #[arg(short = 'f', long = "filename")]
        file: String,
        /// Apply partitioning/mount changes (default validates only)
        #[arg(long)]
        apply: bool,
        /// Command timeout in seconds
        #[arg(long = "timeout-seconds", default_value_t = 300)]
        timeout_seconds: i32,
        /// Skip the automatic nixos-rebuild test+switch after apply
        #[arg(long)]
        no_rebuild: bool,
    },
    /// Upload a local image file to a node image cache
    UploadImage {
        /// Local image filename (qcow2 or raw)
        #[arg(short = 'f', long = "filename")]
        file: String,
        /// Optional destination filename on node
        #[arg(long)]
        name: Option<String>,
        /// Image format (defaults to extension-based inference)
        #[arg(long, value_enum)]
        format: Option<NodeImageFormat>,
        /// Optional expected SHA256 checksum
        #[arg(long = "image-sha256")]
        image_sha256: Option<String>,
    },
}

#[derive(Subcommand)]
enum PullResource {
    /// Pull an image to a node
    Image {
        /// Image URI (HTTPS)
        uri: String,
        /// Required SHA256 checksum for integrity verification
        #[arg(long)]
        sha256: Option<String>,
    },
}

#[derive(Subcommand)]
enum OperatorAction {
    /// Register an operator identity (CN=kctl:<name>) — initial bootstrap uses legacy `kctl` cert
    Create {
        /// Operator name (ASCII alphanumeric, `_`, `-`)
        name: String,
    },
    /// Delete an operator and all role assignments
    Delete { name: String },
    /// List operators
    List,
    /// Show one operator
    Get { name: String },
    /// Issue a client certificate from the controller sub-CA and write `~/.kcore/operators/<name>/operator.{crt,key}`
    #[command(name = "issue-cert")]
    IssueCert { name: String },
    /// Grant a role to an operator
    #[command(name = "grant-role")]
    GrantRole {
        name: String,
        #[arg(long)]
        role: OperatorRoleArg,
    },
    /// Revoke a role from an operator
    #[command(name = "revoke-role")]
    RevokeRole {
        name: String,
        #[arg(long)]
        role: OperatorRoleArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OperatorRoleArg {
    #[value(name = "read-only")]
    ReadOnly,
    #[value(name = "backup-operator")]
    BackupOperator,
    #[value(name = "vm-admin")]
    VmAdmin,
    #[value(name = "cluster-admin")]
    ClusterAdmin,
}

#[derive(Subcommand)]
enum KubernetesAction {
    /// Apply a Kubernetes cluster manifest from file
    Apply {
        /// YAML manifest file
        #[arg(short = 'f', long)]
        file: String,
    },
    /// Get a Kubernetes cluster
    Get {
        /// Cluster name
        name: String,
    },
    /// List all Kubernetes clusters
    List,
    /// Delete a Kubernetes cluster
    Delete {
        /// Cluster name
        name: String,
        /// Force delete even if nodes are unreachable
        #[arg(long)]
        force: bool,
    },
    /// Get cluster health status
    Health {
        /// Cluster name
        name: String,
    },
    /// Scale a Kubernetes cluster (add/remove nodes)
    Scale {
        /// Cluster name
        name: String,
        /// Add control plane nodes (node IDs, comma-separated)
        #[arg(long = "add-control-plane")]
        add_control_plane: Option<String>,
        /// Add worker nodes (node IDs, comma-separated)
        #[arg(long = "add-workers")]
        add_workers: Option<String>,
        /// Remove nodes (node IDs, comma-separated)
        #[arg(long = "remove")]
        remove: Option<String>,
    },
}

#[derive(Subcommand)]
enum SshKeyAction {
    /// Create an SSH key
    Create {
        /// Key name
        name: String,
        /// Public key content (e.g., "ssh-rsa AAAA... user@host")
        #[arg(long = "public-key")]
        public_key: String,
    },
    /// Delete an SSH key
    Delete {
        /// Key name
        name: String,
    },
    /// List all SSH keys
    List,
    /// Get SSH key details
    Get {
        /// Key name
        name: String,
    },
}

#[derive(Subcommand)]
enum GpuAction {
    /// List AI and platform GPUs. Use --local to read this machine.
    List {
        /// Only GPUs on this node
        #[arg(long)]
        node: Option<String>,
        /// Scan PCI devices on the machine where kctl is running
        #[arg(long)]
        local: bool,
    },
}

#[derive(Subcommand)]
enum PciAction {
    /// List GPUs, NICs, NVMe, and raw PCI groups.
    List {
        /// Only devices on this node
        #[arg(long)]
        node: Option<String>,
        /// Scan PCI devices on the machine where kctl is running
        #[arg(long)]
        local: bool,
        /// gpu, nic, nvme, or raw
        #[arg(long = "type")]
        kind: Option<String>,
    },
}

#[derive(Subcommand)]
enum DrainResource {
    /// Drain a node
    Node {
        /// Node ID to drain
        node_id: String,
        /// Target node to move VMs to (optional; auto-schedules if empty)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
}

#[derive(Subcommand)]
enum MigrateResource {
    /// Move a VM to another node. A GPU VM is cold-migrated onto a destination GPU.
    Vm {
        /// VM id or name
        vm_id: String,
        /// Destination node id (or address)
        #[arg(long = "target-node")]
        target_node: String,
        /// Fall back to cold reassignment if live migrate fails
        #[arg(long = "allow-cold-fallback")]
        allow_cold_fallback: bool,
        /// GPU to attach on the destination (repeatable).
        /// Omit this to take a free GPU of the same family.
        /// Example: --gpu radeon0
        #[arg(long = "gpu")]
        gpu: Vec<String>,
        /// vGPU slice to attach on the destination (repeatable).
        /// Omit this to take a free slice of the same profile.
        /// Example: --vgpu nvidia1=A2-4C
        #[arg(long = "vgpu")]
        vgpu: Vec<String>,
        /// NIC to attach on the destination (repeatable).
        /// Omit this to take a free NIC of the same family.
        #[arg(long = "nic")]
        nic: Vec<String>,
        /// NVMe controller to attach on the destination (repeatable).
        #[arg(long = "nvme")]
        nvme: Vec<String>,
    },
    /// Clear a stranded live-migrate receive session on a node.
    ///
    /// Reports what it would clear and does nothing unless --force is given.
    /// Clearing a session whose receive VMM is still alive kills an in-flight
    /// migration; inspect with `kctl get migrate-session <vm>` first.
    #[command(name = "reset-session")]
    ResetSession {
        /// VM id or name
        vm_id: String,
        /// Node holding the stranded session
        #[arg(long)]
        node: String,
        /// Actually clear it. Without this the command only reports.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum WorkloadAction {
    /// Create a workload (vm or container)
    Create {
        /// Workload kind
        #[arg(long, default_value = "container")]
        kind: String,
        /// Workload name
        name: String,
        /// Image (required for container, optional for vm URL mode)
        #[arg(long)]
        image: Option<String>,
        /// CPU cores
        #[arg(long, default_value_t = 2)]
        cpu: i32,
        /// Memory size (e.g. 2G)
        #[arg(long, default_value = "2G")]
        memory: String,
        /// Network name
        #[arg(long)]
        network: Option<String>,
        /// Target node id/address
        #[arg(long = "target-node")]
        target_node: Option<String>,
        /// Storage backend
        #[arg(long = "storage-backend", value_enum, default_value_t = StorageBackend::Filesystem)]
        storage_backend: StorageBackend,
        /// Storage size bytes
        #[arg(long = "storage-size-bytes", default_value_t = 0)]
        storage_size_bytes: i64,
    },
    /// List workloads
    List {
        /// Kind filter: vm, container, all
        #[arg(long, default_value = "all")]
        kind: String,
        /// Target node filter
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Get a workload by id/name
    Get {
        /// Kind: vm or container
        #[arg(long)]
        kind: String,
        /// Workload id or name
        id: String,
        /// Target node filter
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Delete workload
    Delete {
        /// Kind: vm or container
        #[arg(long)]
        kind: String,
        /// Workload id or name
        id: String,
        /// Target node filter
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Start workload
    Start {
        /// Kind: vm or container
        #[arg(long)]
        kind: String,
        /// Workload id or name
        id: String,
        /// Target node filter
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Stop workload
    Stop {
        /// Kind: vm or container
        #[arg(long)]
        kind: String,
        /// Workload id or name
        id: String,
        /// Target node filter
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
}

#[derive(Subcommand)]
enum SecurityGroupAction {
    /// Create security group from YAML manifest
    Create {
        /// SecurityGroup manifest file
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// Apply security group manifest (reconciles attachments)
    Apply {
        /// SecurityGroup manifest file
        #[arg(short = 'f', long = "filename")]
        file: String,
    },
    /// List security groups
    List,
    /// Get security group details
    Get {
        /// Security group name
        name: String,
    },
    /// Delete security group
    Delete {
        /// Security group name
        name: String,
    },
    /// Attach security group to VM or network
    Attach {
        /// Security group name
        #[arg(long)]
        name: String,
        /// Target kind: vm|network
        #[arg(long)]
        kind: String,
        /// Target id (vm id/name or network name)
        #[arg(long)]
        target: String,
        /// Target node (required for network attach)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
    /// Detach security group from VM or network
    Detach {
        /// Security group name
        #[arg(long)]
        name: String,
        /// Target kind: vm|network
        #[arg(long)]
        kind: String,
        /// Target id (vm id/name or network name)
        #[arg(long)]
        target: String,
        /// Target node (required for network detach)
        #[arg(long = "target-node")]
        target_node: Option<String>,
    },
}

#[derive(Clone, ValueEnum)]
enum NodeImageFormat {
    Raw,
    Qcow2,
}

#[derive(Clone, ValueEnum)]
enum StorageBackend {
    Filesystem,
    Lvm,
    Zfs,
    Ceph,
}

fn resolve_controller(cli: &Cli) -> Result<config::ConnectionInfo, String> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    config::resolve_controller(
        &config_path,
        &cli.controller,
        cli.insecure,
        cli.tls_server_name.as_deref(),
        cli.operator_as.as_deref(),
    )
}

fn resolve_node(cli: &Cli) -> Result<config::ConnectionInfo, String> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    config::resolve_node(
        &config_path,
        &cli.node,
        cli.insecure,
        cli.tls_server_name.clone(),
        cli.operator_as.as_deref(),
    )
}

#[tokio::main]
async fn main() {
    install_crypto_provider();
    let cli = Cli::parse();

    let result = match &cli.command {
        Command::Create {
            resource:
                CreateResource::Vm {
                    name,
                    file,
                    cpu,
                    memory,
                    image,
                    image_sha256,
                    image_path,
                    image_format,
                    network,
                    extra_networks,
                    target_node,
                    wait,
                    wait_for_ssh,
                    wait_timeout_seconds,
                    ssh_port,
                    ssh_probe_timeout_ms,
                    ssh_keys,
                    ssh_public_keys,
                    cloud_init_user_data_file,
                    username,
                    password,
                    compliant,
                    storage_backend,
                    storage_size_bytes,
                    target_dc,
                    gpu,
                    vgpu,
                    nic,
                    nvme,
                    pci,
                    node_labels,
                    vm_label,
                    anti_affinity,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::create(
                &info,
                commands::vm::CreateArgs {
                    name: name.clone(),
                    filename: file.clone(),
                    cpu: *cpu,
                    memory: memory.clone(),
                    image: image.clone(),
                    image_sha256: image_sha256.clone(),
                    image_path: image_path.clone(),
                    image_format: image_format.clone(),
                    network: network.clone(),
                    extra_networks: extra_networks.clone(),
                    target_node: target_node.clone(),
                    wait: *wait,
                    wait_for_ssh: *wait_for_ssh,
                    wait_timeout_seconds: *wait_timeout_seconds,
                    ssh_port: *ssh_port,
                    ssh_probe_timeout_ms: *ssh_probe_timeout_ms,
                    ssh_keys: ssh_keys.clone(),
                    ssh_public_keys: ssh_public_keys.clone(),
                    cloud_init_user_data_file: cloud_init_user_data_file.clone(),
                    username: username.clone(),
                    password: password.clone(),
                    compliant: *compliant,
                    storage_backend: storage_backend.as_ref().map(|v| match v {
                        StorageBackend::Filesystem => "filesystem".to_string(),
                        StorageBackend::Lvm => "lvm".to_string(),
                        StorageBackend::Zfs => "zfs".to_string(),
                        StorageBackend::Ceph => "ceph".to_string(),
                    }),
                    storage_size_bytes: *storage_size_bytes,
                    target_dc: target_dc.clone(),
                    gpu: gpu.clone(),
                    vgpu: vgpu.clone(),
                    nic: nic.clone(),
                    nvme: nvme.clone(),
                    pci: pci.clone(),
                    node_labels: node_labels.clone(),
                    labels: vm_label.clone(),
                    anti_affinity: anti_affinity.clone(),
                },
            )
            .await
        }
        Command::Create {
            resource:
                CreateResource::Container {
                    name,
                    image,
                    network,
                    ports,
                    env,
                    cmd,
                },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::container::create(
                &info,
                name,
                image,
                network.as_deref(),
                ports.clone(),
                env.clone(),
                cmd.clone(),
            )
            .await
        }
        Command::Create {
            resource:
                CreateResource::Network {
                    name,
                    external_ip,
                    gateway_ip,
                    internal_netmask,
                    target_node,
                    vlan_id,
                    network_type,
                    no_outbound_nat,
                    ipv6_prefix,
                    ipv6_gateway,
                    east_west_firewall,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::network::create(
                &info,
                commands::network::CreateArgs {
                    name: name.clone(),
                    external_ip: external_ip.clone(),
                    gateway_ip: gateway_ip.clone(),
                    internal_netmask: internal_netmask.clone(),
                    target_node: target_node.clone(),
                    vlan_id: *vlan_id,
                    network_type: network_type.clone(),
                    enable_outbound_nat: !*no_outbound_nat,
                    ipv6_prefix: ipv6_prefix.clone().unwrap_or_default(),
                    ipv6_gateway: ipv6_gateway.clone().unwrap_or_default(),
                    east_west_firewall: *east_west_firewall,
                },
            )
            .await
        }
        Command::Create {
            resource:
                CreateResource::Postgresql {
                    name,
                    file,
                    database,
                    package,
                    port,
                    target_node,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(file) = file {
                commands::postgresql::create_from_manifest(&info, file).await
            } else {
                let Some(name) = name.as_deref() else {
                    fatal("postgresql name is required unless -f is set");
                };
                commands::postgresql::create(
                    &info,
                    name,
                    database.as_deref(),
                    package,
                    *port,
                    target_node.as_deref(),
                )
                .await
            }
        }
        Command::Create {
            resource:
                CreateResource::Volume {
                    name,
                    size_bytes,
                    storage_class,
                    node,
                    volume_name,
                    vm,
                    from_snapshot,
                    encrypt,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::create(
                &info,
                name,
                *size_bytes,
                storage_class,
                node.as_deref(),
                volume_name.as_deref(),
                vm.clone(),
                from_snapshot.clone(),
                *encrypt,
            )
            .await
        }
        Command::Create {
            resource: CreateResource::VolumeSnapshot { volume, name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::snapshot(&info, volume, name.clone()).await
        }
        Command::Create {
            resource:
                CreateResource::SnapshotPolicy {
                    name,
                    vm,
                    volume,
                    schedule,
                    keep,
                    disabled,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::create_snapshot_policy(
                &info,
                name,
                vm.clone(),
                volume.clone(),
                schedule.clone(),
                *keep,
                !*disabled,
            )
            .await
        }
        Command::Create {
            resource: CreateResource::SharedFilesystem { file },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::shared_filesystem::apply_from_file(&info, file).await
        }
        Command::Create {
            resource: CreateResource::ObjectStore { file },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::object_store::apply_from_file(&info, file).await
        }
        Command::Create {
            resource:
                CreateResource::Cluster {
                    controller,
                    certs_dir,
                    context,
                    force,
                },
        } => {
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(config::default_config_path);
            let certs_path = certs_dir
                .clone()
                .unwrap_or_else(|| config::default_cluster_certs_dir(context));
            commands::cluster::create(&config_path, controller, &certs_path, context, *force)
        }

        Command::Delete {
            resource:
                DeleteResource::Vm {
                    vm_id,
                    target_node,
                    delete_data_volumes,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::delete(&info, vm_id, target_node.clone(), *delete_data_volumes).await
        }
        Command::Delete {
            resource: DeleteResource::Volume { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::delete(&info, name).await
        }
        Command::Delete {
            resource: DeleteResource::VolumeSnapshot { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::delete_snapshot(&info, name).await
        }
        Command::Delete {
            resource: DeleteResource::SnapshotPolicy { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::delete_snapshot_policy(&info, name).await
        }
        Command::Delete {
            resource: DeleteResource::Network { name, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::network::delete(&info, name, target_node.clone()).await
        }
        Command::Delete {
            resource: DeleteResource::Postgresql { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::postgresql::delete(&info, name).await
        }

        Command::Delete {
            resource: DeleteResource::Image { name, force },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::image::delete(&info, name, *force).await
        }
        Command::Delete {
            resource: DeleteResource::Container { name, force },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::container::delete(&info, name, *force).await
        }
        Command::Delete {
            resource: DeleteResource::DiskLayout { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::disk_layout::delete(&info, name).await
        }
        Command::Delete {
            resource: DeleteResource::CephCluster { name, force },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ceph_cluster::delete(&info, name, *force).await
        }
        Command::Delete {
            resource: DeleteResource::SharedFilesystem { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::shared_filesystem::delete(&info, name).await
        }
        Command::Delete {
            resource: DeleteResource::ObjectStore { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::object_store::delete(&info, name).await
        }

        Command::Start {
            resource: StartResource::Vm { vm_id, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::start(&info, vm_id, target_node.clone()).await
        }
        Command::Start {
            resource: StartResource::Container { name },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::container::start(&info, name).await
        }

        Command::Stop {
            resource: StopResource::Vm { vm_id, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::stop(&info, vm_id, target_node.clone()).await
        }
        Command::Stop {
            resource: StopResource::Container { name },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::container::stop(&info, name).await
        }

        Command::Attach {
            resource: AttachResource::Volume { name, vm },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::attach(&info, name, vm).await
        }
        Command::Detach {
            resource: DetachResource::Volume { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::detach(&info, name).await
        }
        Command::Cancel {
            resource: CancelOpResource::Operation { id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operation::cancel(&info, id).await
        }

        Command::Restore {
            resource: RestoreResource::Volume { name, snapshot },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::restore(&info, name, snapshot).await
        }
        Command::Restore {
            resource: RestoreResource::Controller { file, confirm },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::cluster_ops::restore(&info, file, *confirm).await
        }
        Command::Backup { output } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::cluster_ops::backup(&info, output).await
        }
        Command::Export {
            resource:
                ExportResource::Volume {
                    name,
                    snapshot,
                    from_snapshot,
                    output,
                    block_size,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::export(
                &info,
                name,
                snapshot,
                from_snapshot.as_deref(),
                output,
                *block_size,
            )
            .await
        }
        Command::Import {
            resource:
                ImportResource::Volume {
                    file,
                    name,
                    base,
                    size_bytes,
                    storage_class,
                    node,
                    punch_zeros,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::import(
                &info,
                file,
                name.as_deref(),
                base.as_deref(),
                *size_bytes,
                storage_class,
                node.as_deref(),
                *punch_zeros,
            )
            .await
        }
        Command::Flatten {
            resource: FlattenResource::Volume { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::flatten(&info, name).await
        }
        Command::Resize {
            resource:
                ResizeResource::Volume {
                    name,
                    size_bytes,
                    grow_filesystem,
                    allow_shrink,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::resize(&info, name, *size_bytes, *grow_filesystem, *allow_shrink)
                .await
        }
        Command::Format {
            resource: FormatResource::Volume { name, fs, force },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::format(&info, name, fs, *force).await
        }
        Command::Mount {
            resource:
                MountResource::Volume {
                    name,
                    path,
                    fs,
                    persistent,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::mount(&info, name, path, fs.as_deref(), *persistent).await
        }
        Command::Unmount {
            resource: UnmountResource::Volume { name, persistent },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::unmount(&info, name, *persistent).await
        }
        Command::Encrypt {
            resource: EncryptResource::Volume { name, destroy_data },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::encrypt(&info, name, *destroy_data).await
        }

        Command::Set {
            resource:
                SetResource::Vm {
                    vm_id,
                    state,
                    target_node,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            let (desired, label) = match state {
                DesiredVmState::Running => {
                    (client::controller_proto::VmDesiredState::Running, "running")
                }
                DesiredVmState::Stopped => {
                    (client::controller_proto::VmDesiredState::Stopped, "stopped")
                }
            };
            commands::vm::set_desired_state(&info, vm_id, desired, target_node.clone(), label).await
        }
        Command::Set {
            resource:
                SetResource::CephDashboardCredentials {
                    name,
                    url,
                    username,
                    password,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ceph_dashboard_credentials::set(&info, name, url, username, password).await
        }

        Command::Update {
            resource:
                UpdateResource::Vm {
                    vm_id,
                    cpu,
                    memory,
                    target_node,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::update(
                &info,
                vm_id,
                *cpu,
                memory.clone(),
                target_node.clone(),
                std::collections::HashMap::new(),
            )
            .await
        }
        Command::Label {
            resource:
                LabelResource::Vm {
                    vm_id,
                    label,
                    target_node,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::label(&info, vm_id, label, target_node.clone()).await
        }
        Command::Update {
            resource: UpdateResource::Cluster { action },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            match action {
                UpdateClusterCmd::Plan { file } => {
                    commands::cluster_update::plan(&info, file).await
                }
                UpdateClusterCmd::Apply { file } => {
                    commands::cluster_update::apply(&info, file).await
                }
                UpdateClusterCmd::Get { name } => commands::cluster_update::get(&info, name).await,
                UpdateClusterCmd::List => commands::cluster_update::list_cmd(&info).await,
                UpdateClusterCmd::Approve { name } => {
                    commands::cluster_update::approve(&info, name).await
                }
                UpdateClusterCmd::Cancel { name } => {
                    commands::cluster_update::cancel(&info, name).await
                }
                UpdateClusterCmd::Rollback { name } => {
                    commands::cluster_update::rollback(&info, name).await
                }
                UpdateClusterCmd::Reboot { name, node } => {
                    commands::cluster_update::reboot(&info, name, node).await
                }
            }
        }

        Command::Get {
            resource: GetResource::Vms { name, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(n) = name {
                commands::vm::get(&info, n, target_node.clone()).await
            } else {
                commands::vm::list(&info, target_node.clone()).await
            }
        }
        Command::Get {
            resource: GetResource::Containers { name },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(name) = name {
                commands::container::get(&info, name).await
            } else {
                commands::container::list(&info).await
            }
        }

        Command::Get {
            resource: GetResource::Nodes { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(n) = name {
                commands::node::get_node(&info, n).await
            } else {
                commands::node::list_nodes(&info).await
            }
        }
        Command::Get {
            resource: GetResource::Postgresql { name, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(name) = name {
                commands::postgresql::get(&info, name).await
            } else {
                commands::postgresql::list(&info, target_node.as_deref()).await
            }
        }
        Command::Get {
            resource: GetResource::Networks { target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::network::list(&info, target_node.clone()).await
        }
        Command::Get {
            resource: GetResource::StorageClass,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::storage_class::list(&info).await
        }
        Command::Get {
            resource: GetResource::Volumes { vm },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::list(&info, vm.clone()).await
        }
        Command::Get {
            resource: GetResource::Volume { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::get(&info, name).await
        }
        Command::Get {
            resource:
                GetResource::Operations {
                    include_finished,
                    vm,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operation::list(&info, *include_finished, vm.clone()).await
        }
        Command::Get {
            resource: GetResource::Operation { id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operation::get(&info, id).await
        }
        Command::Get {
            resource: GetResource::Snapshots { volume },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::volume::list_snapshots(&info, volume.clone()).await
        }
        Command::Get {
            resource: GetResource::SnapshotPolicies { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            if let Some(n) = name {
                commands::volume::get_snapshot_policy(&info, n).await
            } else {
                commands::volume::list_snapshot_policies(&info).await
            }
        }
        Command::Get {
            resource: GetResource::ComplianceReport,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::compliance::report(&info).await
        }
        Command::Get {
            resource: GetResource::ClusterHealth,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::cluster_ops::health(&info).await
        }
        Command::Get {
            resource: GetResource::CryptoConfig,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::compliance::crypto_config(&info).await
        }
        Command::Get {
            resource: GetResource::Sbom { kind, output },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::compliance::export_sbom(&info, kind, output.as_deref()).await
        }
        Command::Get {
            resource:
                GetResource::AuditEvents {
                    limit,
                    action,
                    since,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::audit::list(&info, *limit, action.clone(), since.clone()).await
        }
        Command::Get {
            resource: GetResource::Conflicts { limit },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::conflict::list(&info, *limit).await
        }
        Command::Get {
            resource: GetResource::ReplicationStatus { require_healthy },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::conflict::status(&info, *require_healthy).await
        }
        Command::Get {
            resource: GetResource::DiskLayouts { target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::disk_layout::list(&info, target_node.as_deref()).await
        }
        Command::Get {
            resource: GetResource::CephClusters,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ceph_cluster::list(&info).await
        }
        Command::Get {
            resource: GetResource::CephDashboardCredentials { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ceph_dashboard_credentials::get(&info, name).await
        }
        Command::Get {
            resource: GetResource::SharedFilesystems,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::shared_filesystem::list(&info).await
        }
        Command::Get {
            resource: GetResource::ObjectStores,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::object_store::list(&info).await
        }

        Command::Describe {
            resource: DescribeResource::Vm { name, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::describe(&info, name, target_node.clone()).await
        }
        Command::Describe {
            resource: DescribeResource::Node { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::get_node(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::Network { name, target_node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::network::describe(&info, name, target_node.clone()).await
        }
        Command::Describe {
            resource: DescribeResource::Postgresql { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::postgresql::get(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::StorageClass { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::storage_class::describe(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::SshKey { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ssh_key::get(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::ComplianceReport,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::compliance::report(&info).await
        }
        Command::Describe {
            resource: DescribeResource::CryptoConfig,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::compliance::crypto_config(&info).await
        }
        Command::Describe {
            resource: DescribeResource::DiskLayout { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::disk_layout::get(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::CephCluster { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ceph_cluster::get(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::SharedFilesystem { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::shared_filesystem::get(&info, name).await
        }
        Command::Describe {
            resource: DescribeResource::ObjectStore { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::object_store::get(&info, name).await
        }

        Command::Gpu { action } => match action {
            GpuAction::List { node, local } => {
                if *local {
                    commands::gpu::list_local()
                } else {
                    let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
                    commands::gpu::list(&info, node.as_deref()).await
                }
            }
        },
        Command::Pci { action } => match action {
            PciAction::List { node, local, kind } => {
                if *local {
                    commands::gpu::list_pci_local(kind.as_deref())
                } else {
                    let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
                    commands::gpu::list_pci(&info, node.as_deref(), kind.as_deref()).await
                }
            }
        },
        Command::Node {
            action: NodeAction::Approve { node_id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::approve(&info, node_id).await
        }

        Command::Node {
            action: NodeAction::Reject { node_id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::reject(&info, node_id).await
        }

        Command::Node {
            action: NodeAction::Cordon { node_id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::cordon(&info, node_id).await
        }

        Command::Node {
            action: NodeAction::Uncordon { node_id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::uncordon(&info, node_id).await
        }

        Command::Node {
            action: NodeAction::Delete { node_id },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::delete(&info, node_id).await
        }

        Command::Node {
            action: NodeAction::Disks,
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::disks(&info).await
        }

        Command::Node {
            action: NodeAction::Nics,
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::nics(&info).await
        }

        Command::Node {
            action: NodeAction::Lvm { action },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            match action {
                NodeLvmAction::Create {
                    id,
                    size_bytes,
                    lv_name,
                } => commands::node::lvm_create(&info, id, *size_bytes, lv_name.as_deref()).await,
                NodeLvmAction::Resize {
                    device,
                    size_bytes,
                    allow_shrink,
                } => commands::node::lvm_resize(&info, device, *size_bytes, *allow_shrink).await,
                NodeLvmAction::Format { device, fs, force } => {
                    commands::node::lvm_format(&info, device, fs, *force).await
                }
                NodeLvmAction::Mount {
                    device,
                    path,
                    fs,
                    persistent,
                } => {
                    commands::node::lvm_mount(&info, device, path, fs.as_deref(), *persistent).await
                }
                NodeLvmAction::Unmount { path, persistent } => {
                    commands::node::lvm_unmount(&info, path, *persistent).await
                }
            }
        }

        Command::Node {
            action: NodeAction::Zfs { action },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            match action {
                NodeZfsAction::Create {
                    id,
                    size_bytes,
                    dataset,
                } => commands::node::zfs_create(&info, id, *size_bytes, dataset.as_deref()).await,
                NodeZfsAction::Resize {
                    device,
                    size_bytes,
                    allow_shrink,
                } => commands::node::lvm_resize(&info, device, *size_bytes, *allow_shrink).await,
                NodeZfsAction::Format { device, fs, force } => {
                    commands::node::lvm_format(&info, device, fs, *force).await
                }
                NodeZfsAction::Mount {
                    device,
                    path,
                    fs,
                    persistent,
                } => {
                    commands::node::lvm_mount(&info, device, path, fs.as_deref(), *persistent).await
                }
                NodeZfsAction::Unmount { path, persistent } => {
                    commands::node::lvm_unmount(&info, path, *persistent).await
                }
            }
        }

        Command::Node {
            action:
                NodeAction::Install {
                    os_disk,
                    data_disk,
                    join_controller,
                    run_controller,
                    data_disk_mode,
                    storage_backend,
                    lvm_vg_name,
                    lvm_lv_prefix,
                    zfs_pool_name,
                    zfs_dataset_prefix,
                    disable_vxlan,
                    dc_id,
                    hostname,
                    node_id,
                },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(config::default_config_path);
            let controller_info = config::resolve_controller(
                &config_path,
                &cli.controller,
                false,
                cli.tls_server_name.as_deref(),
                cli.operator_as.as_deref(),
            )
            .ok();
            let certs_dir = config::resolve_install_certs_dir(&config_path).unwrap_or_else(|e| {
                fatal(&format!("resolving install cert directory: {e}"));
            });
            commands::node::install(
                &info,
                controller_info.as_ref(),
                os_disk,
                data_disk.clone(),
                join_controller,
                *run_controller,
                data_disk_mode,
                storage_backend.as_ref().map(|v| match v {
                    StorageBackend::Filesystem => "filesystem",
                    StorageBackend::Lvm => "lvm",
                    StorageBackend::Zfs => "zfs",
                    StorageBackend::Ceph => "ceph",
                }),
                lvm_vg_name.as_deref(),
                lvm_lv_prefix.as_deref(),
                zfs_pool_name.as_deref(),
                zfs_dataset_prefix.as_deref(),
                &certs_dir,
                *disable_vxlan,
                dc_id,
                hostname.as_deref(),
                node_id.as_deref(),
                &config_path,
            )
            .await
        }

        Command::Node {
            action: NodeAction::ApplyNix { file, no_rebuild },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::apply_nix(&info, file, !no_rebuild).await
        }
        Command::Node {
            action:
                NodeAction::ApplyDisk {
                    file,
                    apply,
                    timeout_seconds,
                    no_rebuild,
                },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::node::apply_disk_layout(&info, file, *apply, *timeout_seconds, !no_rebuild)
                .await
        }
        Command::Node {
            action:
                NodeAction::UploadImage {
                    file,
                    name,
                    format,
                    image_sha256,
                },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            let fmt = format.as_ref().map(|f| match f {
                NodeImageFormat::Raw => "raw",
                NodeImageFormat::Qcow2 => "qcow2",
            });
            commands::node::upload_image(&info, file, name.as_deref(), fmt, image_sha256.as_deref())
                .await
        }

        Command::Pull {
            resource: PullResource::Image { uri, sha256 },
        } => {
            let info = resolve_node(&cli).unwrap_or_else(|e| fatal(&e));
            commands::image::pull(&info, uri, sha256.as_deref()).await
        }

        Command::Kubernetes {
            action: KubernetesAction::Apply { file },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::kubernetes::apply_from_file(&info, file).await
        }

        Command::Kubernetes {
            action: KubernetesAction::Get { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::kubernetes::get(&info, name).await
        }

        Command::Kubernetes {
            action: KubernetesAction::List,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::kubernetes::list(&info).await
        }

        Command::Kubernetes {
            action: KubernetesAction::Delete { name, force },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::kubernetes::delete(&info, name, *force).await
        }

        Command::Kubernetes {
            action: KubernetesAction::Health { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::kubernetes::health(&info, name).await
        }

        Command::Kubernetes {
            action:
                KubernetesAction::Scale {
                    name,
                    add_control_plane,
                    add_workers,
                    remove,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            let add_cp = add_control_plane
                .clone()
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default();
            let add_wk = add_workers
                .clone()
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default();
            let rem = remove
                .clone()
                .map(|s| s.split(',').map(String::from).collect())
                .unwrap_or_default();
            commands::kubernetes::scale(&info, name, add_cp, add_wk, rem).await
        }

        Command::SshKey {
            action: SshKeyAction::Create { name, public_key },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ssh_key::create(&info, name, public_key).await
        }
        Command::SshKey {
            action: SshKeyAction::Delete { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ssh_key::delete(&info, name).await
        }
        Command::SshKey {
            action: SshKeyAction::List,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ssh_key::list(&info).await
        }
        Command::SshKey {
            action: SshKeyAction::Get { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::ssh_key::get(&info, name).await
        }

        Command::Operator {
            action: OperatorAction::Create { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::create(&info, name).await
        }
        Command::Operator {
            action: OperatorAction::Delete { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::delete(&info, name).await
        }
        Command::Operator {
            action: OperatorAction::List,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::list(&info).await
        }
        Command::Operator {
            action: OperatorAction::Get { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::get(&info, name).await
        }
        Command::Operator {
            action: OperatorAction::IssueCert { name },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::issue_cert(&info, name).await
        }
        Command::Operator {
            action: OperatorAction::GrantRole { name, role },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::grant_role(&info, name, *role).await
        }
        Command::Operator {
            action: OperatorAction::RevokeRole { name, role },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::operator::revoke_role(&info, name, *role).await
        }

        Command::Drain {
            resource:
                DrainResource::Node {
                    node_id,
                    target_node,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            let mut client = client::controller_client(&info)
                .await
                .unwrap_or_else(|e| fatal(&format!("{e}")));
            let resp = client
                .drain_node(client::controller_proto::DrainNodeRequest {
                    node_id: node_id.to_string(),
                    target_node: target_node.clone().unwrap_or_default(),
                })
                .await;
            match resp {
                Ok(r) => {
                    let r = r.into_inner();
                    println!("{}", r.message);
                    if r.success {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("drain had errors"))
                    }
                }
                Err(e) => Err(anyhow::anyhow!("drain failed: {e}")),
            }
        }

        Command::Migrate {
            resource:
                MigrateResource::Vm {
                    vm_id,
                    target_node,
                    allow_cold_fallback,
                    gpu,
                    nic,
                    nvme,
                    vgpu,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            let mut client = client::controller_client(&info)
                .await
                .unwrap_or_else(|e| fatal(&format!("{e}")));
            let mut devices = gpu.clone();
            devices.extend(nic.clone());
            devices.extend(nvme.clone());
            let resp = client
                .migrate_vm(client::controller_proto::MigrateVmRequest {
                    vm_id: vm_id.to_string(),
                    target_node: target_node.to_string(),
                    allow_cold_fallback: *allow_cold_fallback,
                    gpus: devices,
                    vgpus: vgpu.clone(),
                })
                .await;
            match resp {
                Ok(r) => {
                    let r = r.into_inner();
                    println!("{} (mode={})", r.message, r.mode);
                    if r.success {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("migrate had errors"))
                    }
                }
                Err(e) => Err(anyhow::anyhow!("migrate failed: {e}")),
            }
        }

        Command::Migrate {
            resource: MigrateResource::ResetSession { vm_id, node, force },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::migrate_session::reset(&info, vm_id, node, *force).await
        }

        Command::Rotate {
            resource:
                RotateResource::Certs {
                    controller,
                    certs_dir,
                },
        } => {
            let config_path = cli
                .config
                .clone()
                .unwrap_or_else(config::default_config_path);
            let certs_path = if let Some(dir) = certs_dir {
                dir.clone()
            } else {
                config::resolve_install_certs_dir(&config_path).unwrap_or_else(|e| fatal(&e))
            };
            let info = config::resolve_controller(
                &config_path,
                &[],
                cli.insecure,
                cli.tls_server_name.as_deref(),
                cli.operator_as.as_deref(),
            )
            .ok();
            commands::certs::rotate(&certs_path, controller, info.as_ref()).await
        }
        Command::Rotate {
            resource: RotateResource::NodeCerts { node, all },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::pki::rotate_node_certs(&info, node.as_deref(), *all).await
        }
        Command::Revoke {
            resource:
                RevokeResource::Cert {
                    serial,
                    subject,
                    node,
                    reason,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::pki::revoke_certificate(
                &info,
                serial.as_deref(),
                subject.as_deref(),
                node.as_deref(),
                reason,
            )
            .await
        }
        Command::Get {
            resource:
                GetResource::Certificates {
                    node,
                    status,
                    expiring_within_days,
                },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::pki::list_certificates(
                &info,
                node.as_deref(),
                status.as_deref(),
                *expiring_within_days,
            )
            .await
        }
        Command::Get {
            resource: GetResource::PkiStatus,
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::pki::pki_status(&info).await
        }
        Command::Get {
            resource: GetResource::Crl { output },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::pki::get_crl(&info, output.as_deref()).await
        }
        Command::Get {
            resource: GetResource::MigrateSession { vm_id, node },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::migrate_session::status(&info, vm_id, node.as_deref()).await
        }
        Command::Rotate {
            resource: RotateResource::SubCa { certs_dir },
        } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            let certs_path = if let Some(dir) = certs_dir {
                dir.clone()
            } else {
                let config_path = cli
                    .config
                    .clone()
                    .unwrap_or_else(config::default_config_path);
                config::resolve_install_certs_dir(&config_path).unwrap_or_else(|e| fatal(&e))
            };
            commands::certs::rotate_sub_ca(&certs_path, &info).await
        }

        Command::Apply { file, dry_run } => {
            // Read + parse the manifest exactly once, then dispatch on the
            // classified kind. Cluster / NodeInstall manifests are bootstrap
            // resources and must NEVER be routed through the controller path
            // — including in `--dry-run`, where resolving a controller would
            // fail spuriously even though the local handler is what we want
            // to invoke.
            let classified = commands::apply::classify_manifest(file)
                .unwrap_or_else(|e| fatal(&format!("{e:#}")));
            match classified.local {
                Some(commands::apply::LocalManifestKind::Cluster) => {
                    if *dry_run {
                        println!("--- dry run ---");
                        print!("{}", classified.content);
                        println!("--- end ---");
                        Ok(())
                    } else {
                        let config_path = cli
                            .config
                            .clone()
                            .unwrap_or_else(config::default_config_path);
                        commands::cluster::create_from_manifest(file, &config_path)
                    }
                }
                Some(commands::apply::LocalManifestKind::NodeInstall) => {
                    if *dry_run {
                        println!("--- dry run ---");
                        print!("{}", classified.content);
                        println!("--- end ---");
                        Ok(())
                    } else {
                        let config_path = cli
                            .config
                            .clone()
                            .unwrap_or_else(config::default_config_path);
                        commands::node::install_from_manifest(file, &config_path).await
                    }
                }
                None => {
                    if *dry_run {
                        // Remote manifests: dry-run only prints YAML; do not
                        // resolve the controller (bootstrap may not have one yet).
                        println!("--- dry run ---");
                        print!("{}", classified.content);
                        println!("--- end ---");
                        Ok(())
                    } else {
                        let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
                        commands::apply::apply(&info, file, false).await
                    }
                }
            }
        }

        Command::Diff { file } => {
            let classified = commands::apply::classify_manifest(file)
                .unwrap_or_else(|e| fatal(&format!("{e:#}")));
            match classified.kind.as_deref().map(str::to_ascii_lowercase) {
                Some(ref k)
                    if matches!(k.as_str(), "disklayout" | "disk-layout" | "disk_layout") =>
                {
                    let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
                    commands::disk_layout::diff_from_file(&info, file).await
                }
                Some(ref k)
                    if matches!(k.as_str(), "cephcluster" | "ceph-cluster" | "ceph_cluster") =>
                {
                    let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
                    commands::ceph_cluster::plan_from_file(&info, file).await
                }
                Some(other) => fatal(&format!(
                    "kctl diff currently supports kind: DiskLayout or CephCluster (got {other})"
                )),
                None => fatal("manifest has no kind: field"),
            }
        }

        Command::Version => {
            println!("kctl {VERSION}");
            Ok(())
        }
        Command::Console { name } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            commands::vm::console(&info, name).await
        }
        Command::Workload { action } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            match action {
                WorkloadAction::Create {
                    kind,
                    name,
                    image,
                    cpu,
                    memory,
                    network,
                    target_node,
                    storage_backend,
                    storage_size_bytes,
                } => {
                    let memory_bytes = match client::parse_size_bytes(memory) {
                        Ok(v) => v,
                        Err(e) => fatal(&format!("invalid --memory value: {e}")),
                    };
                    let storage_backend = match storage_backend {
                        StorageBackend::Filesystem => {
                            client::controller_proto::StorageBackendType::Filesystem as i32
                        }
                        StorageBackend::Lvm => {
                            client::controller_proto::StorageBackendType::Lvm as i32
                        }
                        StorageBackend::Zfs => {
                            client::controller_proto::StorageBackendType::Zfs as i32
                        }
                        StorageBackend::Ceph => {
                            client::controller_proto::StorageBackendType::Ceph as i32
                        }
                    };
                    commands::workload::create(
                        &info,
                        kind,
                        name,
                        image.as_deref(),
                        *cpu,
                        memory_bytes,
                        network.as_deref(),
                        target_node.as_deref(),
                        storage_backend,
                        *storage_size_bytes,
                    )
                    .await
                }
                WorkloadAction::List { kind, target_node } => {
                    commands::workload::list(&info, kind, target_node.as_deref()).await
                }
                WorkloadAction::Get {
                    kind,
                    id,
                    target_node,
                } => commands::workload::get(&info, kind, id, target_node.as_deref()).await,
                WorkloadAction::Delete {
                    kind,
                    id,
                    target_node,
                } => commands::workload::delete(&info, kind, id, target_node.as_deref()).await,
                WorkloadAction::Start {
                    kind,
                    id,
                    target_node,
                } => {
                    commands::workload::set_state(&info, kind, id, true, target_node.as_deref())
                        .await
                }
                WorkloadAction::Stop {
                    kind,
                    id,
                    target_node,
                } => {
                    commands::workload::set_state(&info, kind, id, false, target_node.as_deref())
                        .await
                }
            }
        }
        Command::SecurityGroup { action } => {
            let info = resolve_controller(&cli).unwrap_or_else(|e| fatal(&e));
            match action {
                SecurityGroupAction::Create { file } => {
                    commands::security_group::create_from_file(&info, file).await
                }
                SecurityGroupAction::Apply { file } => {
                    commands::security_group::apply_from_file(&info, file).await
                }
                SecurityGroupAction::List => commands::security_group::list(&info).await,
                SecurityGroupAction::Get { name } => {
                    commands::security_group::get(&info, name).await
                }
                SecurityGroupAction::Delete { name } => {
                    commands::security_group::delete(&info, name).await
                }
                SecurityGroupAction::Attach {
                    name,
                    kind,
                    target,
                    target_node,
                } => {
                    commands::security_group::attach(
                        &info,
                        name,
                        kind,
                        target,
                        target_node.as_deref(),
                    )
                    .await
                }
                SecurityGroupAction::Detach {
                    name,
                    kind,
                    target,
                    target_node,
                } => {
                    commands::security_group::detach(
                        &info,
                        name,
                        kind,
                        target,
                        target_node.as_deref(),
                    )
                    .await
                }
            }
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        process::exit(1);
    }
}

fn fatal(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(1);
}
