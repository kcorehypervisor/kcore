# VM Creation Modes

This guide shows all supported ways to create VMs with `kctl`, including
image source modes, auth/cloud-init modes, and readiness options.

## Prerequisites

- Controller reachable (or configured context)
- At least one approved node
- Required storage flags for VM create:
  - `--storage-backend <filesystem|lvm|zfs>`
  - `--storage-size-bytes <bytes>`

## 1) HTTPS image URL + SHA256 (recommended image source mode)

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --network default \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960
```

Use this when you want deterministic image verification and automatic node-side
image caching.

## 2) Node-local image path mode

```bash
kcore-kctl create vm web-01 \
  --image-path /var/lib/kcore/images/debian12-base.qcow2 \
  --image-format qcow2 \
  --network default \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960
```

Use this when image files are already present on the target node.

## 3) Create from YAML manifest

```bash
kcore-kctl create vm -f vm.yaml
```

Minimal manifest example:

```yaml
kind: VM
metadata:
  name: web-01
spec:
  cpu: 2
  memoryBytes: 2G
  imageSha256: "<sha256>"
  disks:
    - backendHandle: "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2"
  nics:
    - network: default
      model: virtio
```

## 4) Compliant mode: username + SSH public key (passwordless)

`--compliant` defaults to `true`, so this is the recommended access model.

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend lvm \
  --storage-size-bytes 21474836480 \
  --username kcore \
  --ssh-public-key "ssh-ed25519 AAAA... user@host"
```

This generates cloud-init user-data that enables key-based login and disables
password auth.

## 5) Use controller-managed SSH key objects (`--ssh-key`)

First register key(s):

```bash
kcore-kctl ssh-key create operator-key \
  --public-key "ssh-ed25519 AAAA... user@host"
```

Then reference by key name during VM create:

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --ssh-key operator-key
```

This also produces passwordless cloud-init behavior.

In HA mode, SSH key create/delete is persisted in controller SQLite and propagated
to peer controllers through CRDT replication events (`ssh_key.create`,
`ssh_key.delete`), so key names converge across controllers.

## 6) Custom cloud-init user-data file

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --cloud-init-user-data-file ./user-data.yaml
```

Use this when you need full control of cloud-init content.

Notes:
- Do not combine `--cloud-init-user-data-file` with `--username`, `--password`,
  or `--ssh-public-key`.
- If `--cloud-init-user-data-file` is supplied, it overrides default/generated
  user-data.

## 7) Non-compliant mode (explicit password login)

Password auth is treated as non-compliant and requires explicit acknowledgment:

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --username kcore \
  --password "temporary-password" \
  --compliant=false
```

Without `--compliant=false`, this mode is rejected.

## 8) Targeting/scheduling options

### Let controller choose a node

```bash
kcore-kctl create vm web-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend lvm \
  --storage-size-bytes 21474836480
```

### Force a specific node

```bash
kcore-kctl create vm web-01 \
  --target-node kvm-node-01 \
  --image https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2 \
  --image-sha256 <sha256> \
  --storage-backend lvm \
  --storage-size-bytes 21474836480
```

## 9) Readiness wait modes

### Wait until VM reaches running state

```bash
kcore-kctl create vm web-01 \
  --image-path /var/lib/kcore/images/debian12-base.qcow2 \
  --image-format qcow2 \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --wait
```

### Wait until SSH is reachable

```bash
kcore-kctl create vm web-01 \
  --image-path /var/lib/kcore/images/debian12-base.qcow2 \
  --image-format qcow2 \
  --storage-backend filesystem \
  --storage-size-bytes 42949672960 \
  --wait-for-ssh \
  --wait-timeout-seconds 300 \
  --ssh-port 22
```

## 10) Common validation rules

- `--image` and `--image-path` are mutually exclusive.
- URL mode requires `https://` and `--image-sha256`.
- Path mode requires `--image-format` (`raw` or `qcow2`) unless inferred.
- `--password` requires `--username`.
- `--ssh-public-key` requires `--username`.
- `--cloud-init-user-data-file` cannot be mixed with generated identity flags.
