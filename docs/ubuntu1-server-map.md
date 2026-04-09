# ubuntu1 Server Map — Pre-NixOS Migration Reference

> **Host:** ubuntu1 / 192.168.40.10
> **OS:** Ubuntu 22.04.5 LTS, kernel 6.8.0-101-generic
> **Hardware:** Gigabyte X570 AORUS ULTRA
> **Gathered:** 2026-04-05

---

## 1. Hardware

| Component | Detail |
|-----------|--------|
| CPU | AMD Ryzen 5 5600G (6C/12T, integrated Radeon) |
| RAM | 64 GB DDR4 (62 Gi usable), no swap |
| GPU | AMD Cezanne (integrated, rev c9) |
| NIC 1 (active) | Intel I211 Gigabit — `enp6s0` — MAC f4:52:14:1f:37:d0 |
| NIC 2 (down) | Mellanox ConnectX-3 Pro — `enp8s0` — MAC 18:c0:4d:07:78:de |
| NIC 3 (down) | Second port of Mellanox — `enp6s0d1` — MAC f4:52:14:1f:37:d1 |
| Wi-Fi (down) | Intel Wi-Fi 6 AX200 — `wlp7s0` + Bluetooth |
| Coral TPU | Google Coral Edge TPU (PCIe) — `/dev/apex_0` — used by Frigate |
| UPS | MGE UPS Systems (USB 0463:ffff) — monitored by upower |
| USB serial | Prolific PL2303 — Bus 007 Device 002 |
| USB misc | Glinet composite, Pixart mouse, IT8297 RGB controller |

---

## 2. Storage Layout

### 2.1 Physical Disks

| Device | Size | Model | Purpose |
|--------|------|-------|---------|
| nvme1n1 | 1.9 TB | Sabrent | **OS disk** (LVM `ubuntu-vg/ubuntu-lv` → `/`, `/boot`, `/boot/efi`) |
| nvme0n1 | 1.9 TB | Sabrent | **Frigate storage** → `/mnt/frigate` (ext4) |
| sda | 3.7 TB | Fanxiang S101Q | md126 member (RAID 0) |
| sdb | 3.7 TB | Fanxiang S101Q | md126 member (RAID 0) |
| sdd | 447 GB | Kingston SA400S37 | md127 member (RAID 0) |
| sde | 894 GB | Kingston SA400S37 | md127 member (RAID 0) |
| sdf | 10.9 TB | Seagate ST12000NM0007 | NTFS data drive (mounted via auto) |
| sdg | 12.7 TB | WD WD140EDFZ | md125 member (RAID 1, degraded) |
| sdh | 12.7 TB | WD WD140EDFZ | md125 member (RAID 1, degraded — `[U_]` = 1 of 2 active) |

### 2.2 RAID Arrays (md)

| Array | Level | Size | Filesystem | Mount | Status |
|-------|-------|------|------------|-------|--------|
| md126 | RAID 0 | 7.5 TB | ext4 | `/mnt/md126` | **CRITICAL — your data** |
| md127 | RAID 0 | 1.3 TB | ext4 | `/var/lib/libvirt/images` | VM images |
| md125 | RAID 1 | 12.7 TB | xfs | **NOT MOUNTED** | Degraded `[U_]` — only sdg active |

### 2.3 Mount Map (/etc/fstab)

```
/dev/dm-uuid-LVM-...          /                        ext4  defaults  0 1
UUID=15de4c48-...              /boot                    ext4  defaults  0 1
UUID=AD80-C019                 /boot/efi                vfat  defaults  0 1
UUID=85469925-...              /mnt/frigate             ext4  defaults  0 1
/dev/md127                     /var/lib/libvirt/images   ext4  defaults,relatime,stripe=256  0 2
UUID=40f2f131-...              /mnt/md126               ext4  defaults  0 2
```

NTFS drive (`sdf1`) auto-mounts to `/media/rtacconi/88D24304D242F5CA`.

### 2.4 /mnt/md126 Contents (the data to preserve)

| Directory | Size | Description |
|-----------|------|-------------|
| movies/ | ~1.5 TB | Jellyfin media library (owned by jellyfin:jellyfin) |
| music/ | 249 GB | Music collection |
| photos/ | 14 GB | Photo library (owned by jellyfin:jellyfin) |
| kcore-rust/ | 60 GB | **This workspace** — Rust/Nix kcore project |
| rubytapas/ | 19 GB | Ruby Tapas video collection |
| backup/ | 17 GB | Backups (dev.zip, poc-platform, tf-kubernetes, etc.) |
| -/ | 11 GB | Misc |
| nix/ | 6.6 GB | Nix store mirror/cache |
| win11-cloud-hypervisor/ | 6.7 GB | Windows 11 cloud-hypervisor experiment |
| downloads/ | 5.6 GB | Downloads |
| rtacconi/ | 5.4 GB | Personal files |
| transmission/ | 4.6 GB | Torrent data |
| InvestorsLive DVD/ | 3.9 GB | Trading course |
| kcore-site/ | 4.9 MB | Kcore website |
| terraform-provider-kcore/ | 456 KB | Terraform provider |
| kore/ | 2.5 MB | Kore project |
| dev/alphavantage/ | ~477 MB | Alpha Vantage project + docker-compose |
| RoonBackups/ | 596 MB | Roon Server backups |
| tvheadend/ | 192 KB | TVHeadend config |
| timemachine/ | 4 KB | Time Machine (empty) |
| **TOTAL** | **~1.9 TB used / 5.3 TB free** | |

### 2.5 NTFS Drive (sdf1 — 10.9 TB Seagate)

Contents (951 GB used): downloads, InvestorsLive DVD, movies, music, nvr, photos, Python Algo Course Certificate, RoonBackups, rubytapas — appears to be a **backup/mirror** of md126 content.

---

## 3. Network

### 3.1 Interfaces

| Interface | IP | State | Purpose |
|-----------|-----|-------|---------|
| enp6s0 | 192.168.40.10/24 | UP | Primary LAN |
| tailscale0 | 100.69.74.106/32 | UP | Tailscale VPN |
| docker0 | 172.17.0.1/16 | UP | Docker default bridge |
| br-806e93699061 | 172.19.0.1/16 | UP | Docker custom bridge |
| br-1446875100c7 | 172.18.0.1/16 | UP | Docker custom bridge |
| virbr0 | 192.168.122.1/24 | DOWN | libvirt NAT bridge |

Default gateway: `192.168.40.1` (MikroTik router)

### 3.2 Tailscale

- **Version:** 1.94.2
- **Tailnet:** `clouded-dragon.ts.net`
- **DNS name:** `ubuntu1.clouded-dragon.ts.net`
- **Tailscale IP:** 100.69.74.106
- **Subnet routes advertised:** `192.168.40.0/24` (exposing the entire LAN)
- **DERP relay:** lhr (London)
- **Other devices on tailnet:** nixos1, homeassistant, glkvm, iphone-13, aleks, aleksanders-macbook-pro, and more

### 3.3 Listening Ports (key services)

| Port | Service |
|------|---------|
| 22 | SSH |
| 25 | Postfix SMTP |
| 53 | dnsmasq (libvirt) + systemd-resolved |
| 80 | nginx (dashboard + reverse proxy) |
| 139, 445 | Samba (SMB) |
| 631 | CUPS printing |
| 1883 | Mosquitto MQTT (localhost only) |
| 1984 | go2rtc WebRTC (Frigate) |
| 2507 | Node.js process (Cursor?) |
| 3389 | xrdp |
| 4000 | Roon ARC? |
| 4200 | Prefect (Alpha Vantage, via Docker) |
| 5000 | Frigate Web UI (Docker) |
| 5432 | PostgreSQL 14 |
| 5900 | x11vnc VNC |
| 8096 | Jellyfin |
| 8182 | Samsung TV Plus for Channels (Docker) |
| 8554, 8555 | go2rtc RTSP/WebRTC (Frigate) |
| 8765 | KVM switch control (proxied at /kvm) |
| 8971 | Frigate API |
| 9004 | ? |
| 9150, 9200, 9330-9332 | Roon Server ports |

---

## 4. Services Inventory

### 4.1 Docker Containers (running)

| Container | Image | Ports | Compose File |
|-----------|-------|-------|-------------|
| frigate | ghcr.io/blakeblackshear/frigate:stable | 5000, 1984, 8554, 8555, 8971 | `/mnt/frigate/docker-compose.yml` |
| alphavantage-prefect-1 | prefecthq/prefect:3-latest | 4200 | `/mnt/md126/dev/alphavantage/docker-compose.yml` |
| samsung-tvplus | matthuisman/samsung-tvplus-for-channels | 8182 | standalone |
| roon-extension-manager | theappgineer/roon-extension-manager:v1.x | — | systemd-managed |

### 4.2 Systemd Services (notable)

| Service | Description | Config/Data |
|---------|-------------|-------------|
| **jellyfin** | Media server | Data: `/var/lib/jellyfin/` (1.5 GB), Config: `/etc/jellyfin/`, Media: `/mnt/md126/movies`, `/mnt/md126/photos`, `/mnt/md126/music` |
| **roonserver** | Roon music server | Binary: `/opt/RoonServer/`, Data: `/var/roon/` (795 MB), Backups: `/mnt/md126/RoonBackups/` |
| **roon-extension-manager** | Roon extensions (Docker) | Config: `/home/rtacconi/.roon-extension-manager/` |
| **nginx** | Reverse proxy + dashboard | Config: `/etc/nginx/sites-enabled/default`, Root: `/var/www/html/` |
| **docker** + **containerd** | Container runtime | — |
| **tailscaled** | Tailscale VPN agent | Subnet router for 192.168.40.0/24 |
| **postgresql@14-main** | PostgreSQL 14 | DBs: `alpaca`, `postgres`. Data: `/var/lib/postgresql/14/main/` |
| **mosquitto** | MQTT broker | Config: `/etc/mosquitto/mosquitto.conf`, localhost only |
| **libvirtd** | KVM/QEMU virtualization | Images: `/var/lib/libvirt/images/` (md127, 40 GB used) |
| **smbd / nmbd** | Samba file sharing | Share `[md126]` → `/mnt/md126` (user: rtacconi) |
| **nix-daemon** | Nix package manager daemon | Store: `/nix/store/` |
| **ssh** | OpenSSH server | Port 22 |
| **x11vnc** | VNC server (macOS Screen Sharing compatible) | Port 5900, password: `/etc/x11vnc.pass` |
| **xrdp / xrdp-sesman** | Remote Desktop Protocol | Port 3389 |
| **nxserver** | NoMachine remote desktop | Ports 4000, 7001, etc. |
| **ivpn-service** | IVPN VPN client | Binary: `/usr/bin/ivpn-service` |
| **gdm** | GNOME Display Manager | Desktop session on :1 |
| **postfix** | Mail transport agent | Port 25 |
| **cups / cups-browsed** | Printing | Port 631, Brother HL-L2370DN at 192.168.40.166 |
| **mdmonitor** | MD RAID monitor | — |
| **snapd** | Snap package manager | See snap list below |
| **smartmontools** | SMART disk monitoring | — |

### 4.3 Snaps Installed

atom, firefox, gh (GitHub CLI), makemkv, vlc, lxd, and base snaps (core18/20/22/24, gnome themes).

### 4.4 libvirt VMs (all shut off)

| VM | Purpose (likely) |
|----|---------|
| cka_master1 | CKA (Kubernetes admin) lab |
| cka_master2 | CKA lab |
| cka_master3 | CKA lab |
| opnsense | OPNsense firewall VM |
| pig | ? |

---

## 5. Web Dashboard Map (http://192.168.40.10/)

Served by nginx from `/var/www/html/index.html`.

### Server & Applications

| Name | URL | Type |
|------|-----|------|
| Frigate NVR | http://192.168.40.10:5000 | Docker |
| Home Assistant | http://192.168.40.125:8123 | External device |
| Jellyfin | http://192.168.40.10:8096 | systemd |
| go2rtc (WebRTC) | http://192.168.40.10:1984 | Docker (part of Frigate) |
| KVM switch | http://192.168.40.10/kvm | nginx proxy → :8765 |
| Alpha Vantage | http://192.168.40.10/alphavantage/ | nginx → static + Prefect + FastAPI |

### Security Cameras

| Name | IP | Brand |
|------|-----|-------|
| Doorbell Camera | 192.168.40.127 (RTSP) | Reolink |
| Driveway Camera | 192.168.40.200 (RTSP) | Reolink |

### Network Infrastructure

| Name | IP | Brand |
|------|-----|-------|
| Main Router (gateway) | 192.168.40.1 | MikroTik |
| Wi-Fi Router | 192.168.40.2 | Linksys |
| Switch/Router 1 | 192.168.40.3 | MikroTik |
| Switch/Router 2 | 192.168.40.4 | MikroTik |
| Switch/Router 3 | 192.168.40.5 | MikroTik |
| Switch/Router 4 | 192.168.40.6 | MikroTik |
| Switch (250GS) | 192.168.40.161 | MikroTik |

### Media & Audio

| Name | IP | Brand |
|------|-----|-------|
| Denon/Marantz AV Receiver | 192.168.40.185:10443 | D&M |

### Other Devices

| Name | IP | Notes |
|------|-----|-------|
| Brother Printer | 192.168.40.166 | HL-L2370DN laser |
| Device (.180) | 192.168.40.180 | MAC 2C:C8:1B |
| Device (.199) | 192.168.40.199 | MAC 28:56:2F |

### Remote Access to ubuntu1

| Method | Endpoint |
|--------|----------|
| VNC | vnc://192.168.40.10:5900 (x11vnc) |
| SSH | ssh rtacconi@192.168.40.10 |
| RDP | 192.168.40.10:3389 (xrdp) |
| NoMachine | 192.168.40.10:4000 |

---

## 6. Frigate NVR Configuration

- **Docker image:** `ghcr.io/blakeblackshear/frigate:stable`
- **Coral TPU:** PCIe (`/dev/apex_0`) — but config currently sets `detectors: cpu1: type: cpu` (not using Coral!)
- **MQTT:** disabled in Frigate config
- **Storage:** `/mnt/frigate/storage/` (628 GB used on nvme0n1)
- **Config:** `/mnt/frigate/config/config.yml`
- **Cameras:**
  - `doorbell` — Reolink at 192.168.40.127, tracks: person, dog, cat, package
  - `driveway` — Reolink at 192.168.40.200, tracks: person, car, dog, cat
- **Retention:** 7 days motion, 14 days alerts/detections/snapshots

---

## 7. Alpha Vantage Stack

Reverse-proxied via nginx under `/alphavantage/`:

| Path | Backend |
|------|---------|
| `/alphavantage/` | Static HTML (burger menu home) at `/var/www/alphavantage/` |
| `/alphavantage/charts/` | React SPA (Vite) at `/var/www/alphavantage/charts/` |
| `/alphavantage/dash/` | Prefect 3 UI (Docker, port 4200) |
| `/api/` | FastAPI charts API (port 8000) — **not currently running?** |

- **Database:** PostgreSQL 14 — database `alpaca`
- **Docker compose:** `/mnt/md126/dev/alphavantage/docker-compose.yml`

---

## 8. Samba Shares

Single share:
```
[md126]
  path = /mnt/md126
  valid users = rtacconi
  read only = no
```

---

## 9. Nix Installation

- **nix-daemon** is running (multi-user install)
- **Nix store:** `/nix/store/` (size TBD, `du` timed out — large)
- **Nix on md126:** `/mnt/md126/nix/nix/` (6.6 GB)
- **kcore-rust flake:** `flake.nix` builds kcore-node-agent, kcore-controller, kcore-kctl, kcore-dashboard; NixOS modules for ISO/VM/services

---

## 10. Migration Checklist — What to Preserve for NixOS

### CRITICAL — Data on /mnt/md126 (md126 RAID 0)

**This is RAID 0 with no redundancy.** Back up before any OS changes.

| What | Location | Action |
|------|----------|--------|
| Movies | `/mnt/md126/movies/` (1.5 TB) | Keep in place |
| Music | `/mnt/md126/music/` (249 GB) | Keep in place |
| Photos | `/mnt/md126/photos/` (14 GB) | Keep in place |
| kcore-rust | `/mnt/md126/kcore-rust/` (60 GB) | Keep in place (git repo) |
| All other dirs | See section 2.4 | Keep in place |

**Strategy:** The OS is on nvme1n1 (LVM). You can reinstall NixOS on nvme1n1 without touching md126 at all. Just ensure `/mnt/md126` is remounted via fstab/disko after install.

### Services to Recreate on NixOS

| Service | Current Setup | NixOS Approach |
|---------|---------------|----------------|
| Jellyfin | apt package, systemd | `services.jellyfin.enable = true;` |
| Frigate | Docker container + Coral | Docker/Podman container in NixOS |
| Roon Server | `/opt/RoonServer/` binary, systemd | Nix package or wrap binary |
| nginx | apt, `/etc/nginx/` | `services.nginx` module |
| Tailscale | apt, systemd | `services.tailscale.enable = true;` + subnet routes |
| Samba | apt, `/etc/samba/smb.conf` | `services.samba` module |
| PostgreSQL 14 | apt | `services.postgresql` module |
| Mosquitto | apt | `services.mosquitto` module |
| Docker | apt | `virtualisation.docker.enable = true;` |
| x11vnc | systemd unit | Custom systemd unit in NixOS |
| xrdp | apt | `services.xrdp` module |
| NoMachine | proprietary binary | Wrap with Nix or skip |
| IVPN | proprietary binary | Wrap with Nix or skip |
| CUPS | apt | `services.printing` module |
| Postfix | apt | `services.postfix` module |
| libvirt/KVM | apt | `virtualisation.libvirtd.enable = true;` |
| Samsung TV Plus | Docker container | Docker container in NixOS |
| Roon Ext Manager | Docker + systemd | Docker container in NixOS |
| Prefect (alphavantage) | Docker container | Docker container in NixOS |
| KVM switch control | process on :8765 | TBD — find source |
| Alpha Vantage charts API | FastAPI on :8000 | Nix service or container |
| Snap packages | snapd | Replace with Nix packages (firefox, gh, vlc, makemkv, atom) |

### Data to Back Up Before Migration

| What | Location | Size | Backup Target |
|------|----------|------|---------------|
| PostgreSQL (alpaca DB) | `/var/lib/postgresql/14/main/` | tiny | `pg_dump` to `/mnt/md126/backup/` |
| Jellyfin metadata | `/var/lib/jellyfin/` | 1.5 GB | Copy to `/mnt/md126/backup/` |
| Roon Server data | `/var/roon/` | 795 MB | Already has `/mnt/md126/RoonBackups/` |
| Frigate config | `/mnt/frigate/config/` | small | Copy to `/mnt/md126/backup/` |
| Frigate recordings | `/mnt/frigate/storage/` | 628 GB | On separate NVMe, survives OS reinstall |
| nginx config | `/etc/nginx/sites-enabled/default` | small | Copy to `/mnt/md126/backup/` |
| Samba config | `/etc/samba/smb.conf` | small | Document in NixOS config |
| Mosquitto config | `/etc/mosquitto/` | small | Document in NixOS config |
| Home directory | `/home/rtacconi/` | TBD | Copy essentials to `/mnt/md126/backup/` |
| x11vnc password | `/etc/x11vnc.pass` | tiny | Copy |
| Tailscale state | `/var/lib/tailscale/` | small | Re-auth after install |
| Docker volumes | various | varies | Back up if needed |
| NTFS drive data | `/media/rtacconi/.../` (sdf1) | 951 GB | Already a backup copy |
| md125 RAID 1 | NOT MOUNTED, degraded | 12.7 TB | Investigate — may have old data |

### Disks That Survive OS Reinstall (not on nvme1n1)

- `/mnt/md126` (md126 = sda+sdb) — **your main data, survives**
- `/mnt/frigate` (nvme0n1p1) — **Frigate storage, survives**
- `/var/lib/libvirt/images` (md127 = sdd+sde) — **VM images, survives** (remount needed)
- NTFS drive (sdf1) — **backup data, survives**
- md125 (sdg+sdh) — **not mounted, survives**

### Backup Plan

1. `pg_dump -U postgres alpaca > /mnt/md126/backup/alpaca-$(date +%F).sql`
2. `cp -a /var/lib/jellyfin/ /mnt/md126/backup/jellyfin/`
3. `cp -a /mnt/frigate/config/ /mnt/md126/backup/frigate-config/`
4. `cp /etc/nginx/sites-enabled/default /mnt/md126/backup/nginx-default.conf`
5. `cp /var/www/html/index.html /mnt/md126/backup/dashboard-index.html`
6. `cp -a /var/www/alphavantage/ /mnt/md126/backup/alphavantage-www/`
7. `cp -a /etc/samba/smb.conf /mnt/md126/backup/smb.conf`
8. `cp /etc/mosquitto/mosquitto.conf /mnt/md126/backup/mosquitto.conf`
9. `cp /etc/x11vnc.pass /mnt/md126/backup/x11vnc.pass`
10. `tar czf /mnt/md126/backup/home-rtacconi.tar.gz /home/rtacconi/`
11. Verify `/mnt/md126/RoonBackups/` is current

---

## 11. Architecture Diagram

```
┌──────────────────────────────────────────────────────────────────────┐
│                        ubuntu1 (192.168.40.10)                       │
│                     Ubuntu 22.04 → NixOS migration                   │
│                                                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌──────────────┐                │
│  │  nvme1n1     │  │  nvme0n1    │  │ sda+sdb      │                │
│  │  1.9 TB      │  │  1.9 TB     │  │ 7.5 TB RAID0 │                │
│  │  OS (LVM)    │  │  /mnt/      │  │ /mnt/md126   │                │
│  │  /           │  │  frigate    │  │ movies,music  │                │
│  │  /boot       │  │  628GB used │  │ photos,code   │                │
│  │  /boot/efi   │  │             │  │ 1.9TB used    │                │
│  └─────────────┘  └─────────────┘  └──────────────┘                │
│                                                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌──────────────┐                │
│  │ sdd+sde     │  │ sdf (NTFS)  │  │ sdg+sdh      │                │
│  │ 1.3TB RAID0 │  │ 10.9 TB     │  │ 12.7TB RAID1 │                │
│  │ /var/lib/   │  │ backup      │  │ NOT MOUNTED  │                │
│  │ libvirt/img │  │ 951GB used  │  │ degraded     │                │
│  └─────────────┘  └─────────────┘  └──────────────┘                │
│                                                                      │
│  Services:                                                           │
│  ├── nginx :80 (dashboard + reverse proxy)                          │
│  ├── Jellyfin :8096 (media server)                                  │
│  ├── Frigate :5000 (NVR, Docker, Coral TPU)                        │
│  ├── go2rtc :1984 (WebRTC streaming)                               │
│  ├── Roon Server :9100+ (music)                                    │
│  ├── PostgreSQL :5432 (alpaca DB)                                  │
│  ├── Tailscale (subnet router 192.168.40.0/24)                     │
│  ├── Samba :445 (/mnt/md126 share)                                 │
│  ├── Mosquitto :1883 (MQTT, localhost)                             │
│  ├── Docker (Frigate, Prefect, Samsung TV+, Roon Ext)             │
│  ├── libvirtd (5 VMs, all shut off)                                │
│  ├── x11vnc :5900 / xrdp :3389 / NoMachine :4000                  │
│  ├── SSH :22                                                        │
│  ├── CUPS :631 (Brother printer)                                   │
│  ├── Postfix :25 (mail)                                            │
│  ├── KVM switch :8765 (→ /kvm)                                    │
│  ├── Alpha Vantage (static + Prefect + FastAPI)                    │
│  └── nix-daemon (multi-user Nix)                                   │
│                                                                      │
│  Network: enp6s0 → 192.168.40.0/24 → MikroTik .1                  │
│  Tailscale: 100.69.74.106 (clouded-dragon.ts.net)                  │
│  Desktop: GNOME (GDM on :1)                                        │
└──────────────────────────────────────────────────────────────────────┘

External devices on 192.168.40.0/24:
  .1    MikroTik main router (gateway)
  .2    Linksys Wi-Fi router
  .3-6  MikroTik switches/routers
  .125  Home Assistant
  .127  Reolink doorbell camera
  .161  MikroTik 250GS switch
  .166  Brother HL-L2370DN printer
  .180  Unknown device (MAC 2C:C8:1B)
  .185  Denon/Marantz AV receiver
  .199  Unknown device (MAC 28:56:2F)
  .200  Reolink driveway camera
```
