# User Documentation

This directory is the starting point for user-facing documentation.

## Documents

- [Add a New Node](./add-new-node.md)
- [Compliance Enablement Evaluation](./compliance-enablement-evaluation.md)
- [Images](./images.md)
- [Licensing](./licensing.md)
- [VM Creation Modes](./vm-creation-modes.md)

## Getting Started

This quickstart shows the first end-to-end user flow:
1. add the first worker node to a cluster,
2. register an operator SSH key,
3. create a Debian 12 VM,
4. connect to the VM with the operator key.

Examples use `kcore-kctl` for VM and SSH key operations (as used in this docs set).

### 1) Add and approve your first node

Follow the full guide in [Add a New Node](./add-new-node.md). Minimal flow:

```bash
# Inspect install target
kctl --node <NEW_NODE_IP>:9091 node disks
kctl --node <NEW_NODE_IP>:9091 node nics

# Install and join controller(s)
kctl --node <NEW_NODE_IP>:9091 node install \
  --os-disk /dev/sda \
  --join-controller <CTRL1_IP>:9090 \
  --join-controller <CTRL2_IP>:9090 \
  --dc-id DC1

# Approve when it appears as pending
kctl get nodes
kctl node approve <NODE_ID>
kctl get nodes
```

### 2) Register the operator SSH public key

```bash
kcore-kctl ssh-key create operator-key \
  --public-key "ssh-ed25519 AAAA... operator@laptop"
```

Use your actual public key content from `~/.ssh/*.pub`.

### 3) Create a Debian 12 VM on the approved node

Get the Debian 12 image SHA256 first, then create the VM:

```bash
kcore-kctl create vm debian12-01 \
  --target-node <NODE_ID> \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <DEBIAN12_QCOW2_SHA256> \
  --network default \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --ssh-key operator-key \
  --wait-for-ssh \
  --wait-timeout-seconds 300
```

For additional creation patterns, see [VM Creation Modes](./vm-creation-modes.md).

### 4) Connect to the VM with the operator key

After the VM is reachable, SSH in with the private key matching the key uploaded above:

```bash
ssh -i ~/.ssh/<operator-private-key> <username>@<vm-ip>
```

If you did not set a custom username via cloud-init flags, check the VM details to
confirm the expected login user and IP before connecting.
