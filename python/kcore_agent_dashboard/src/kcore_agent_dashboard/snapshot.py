"""Load the dashboard snapshot and shape it into table rows."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any

_KEYS = (
    "compliance",
    "vms",
    "volumes",
    "networks",
    "network_overview",
    "storage",
    "ceph_clusters",
    "shared_filesystems",
    "object_stores",
    "operations",
    "audit",
    "replication",
    "conflicts",
)


def empty_snapshot(warning: str) -> dict[str, Any]:
    snap: dict[str, Any] = {
        "source": "unavailable",
        "warnings": [warning] if warning else [],
    }
    objects = {"compliance", "network_overview", "storage", "replication"}
    for key in _KEYS:
        snap[key] = None if key in objects else []
    return snap


def normalize(raw: dict[str, Any] | None) -> dict[str, Any]:
    raw = raw or {}
    snap = empty_snapshot("")
    snap["warnings"] = [str(item) for item in raw.get("warnings") or []]
    snap["source"] = str(raw.get("source") or "unavailable")
    for key in _KEYS:
        if key in raw:
            snap[key] = raw[key]
    return snap


def _s(value: Any, default: str = "") -> str:
    if value is None or value == "":
        return default
    if isinstance(value, bool):
        return "yes" if value else "no"
    return str(value)


def _rows(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, dict)]


def vm_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "state": _s(row.get("state")),
            "node_id": _s(row.get("node_id")),
            "storage_backend": _s(row.get("storage_backend")),
            "cpu": _s(row.get("cpu")),
            "memory": _s(row.get("memory")),
            "id": _s(row.get("id")),
        }
        for row in _rows(snap.get("vms"))
    ]


def volume_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "backend": _s(row.get("backend")),
            "pool": _s(row.get("pool")),
            "node_id": _s(row.get("node_id")),
            "role": _s(row.get("role")),
            "attach_state": _s(row.get("attach_state")),
            "size": _s(row.get("size")),
            "vm": _s(row.get("vm")),
            "encrypted": _s(row.get("encrypted")),
        }
        for row in _rows(snap.get("volumes"))
    ]


def network_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "network_type": _s(row.get("network_type")),
            "node_id": _s(row.get("node_id")),
            "external_ip": _s(row.get("external_ip")),
            "gateway_ip": _s(row.get("gateway_ip")),
            "vlan_id": _s(row.get("vlan_id")),
        }
        for row in _rows(snap.get("networks"))
    ]


def iface_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    overview = snap.get("network_overview")
    if not isinstance(overview, dict):
        return []
    rows: list[dict[str, str]] = []
    for node in _rows(overview.get("nodes")):
        node_name = _s(node.get("hostname") or node.get("node_id"))
        for iface in _rows(node.get("interfaces")):
            addresses = iface.get("addresses") or []
            if not isinstance(addresses, list):
                addresses = [addresses]
            rows.append(
                {
                    "node": node_name,
                    "node_id": _s(node.get("node_id")),
                    "name": _s(iface.get("name")),
                    "kind": _s(iface.get("kind")),
                    "state": _s(iface.get("state")),
                    "addresses": ", ".join(_s(item) for item in addresses),
                    "mac": _s(iface.get("mac_address")),
                }
            )
    return rows


def ceph_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "phase": _s(row.get("phase")),
            "health_message": _s(row.get("health_message")),
            "public_network": _s(row.get("public_network")),
            "members": _s(row.get("members")),
            "size": _s(row.get("size")),
            "osd_devices": _s(row.get("osd_devices")),
        }
        for row in _rows(snap.get("ceph_clusters"))
    ]


def filesystem_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "ceph_cluster": _s(row.get("ceph_cluster")),
            "phase": _s(row.get("phase")),
            "health_message": _s(row.get("health_message")),
            "quota": _s(row.get("quota")),
            "clients": _s(row.get("clients")),
        }
        for row in _rows(snap.get("shared_filesystems"))
    ]


def object_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "name": _s(row.get("name")),
            "ceph_cluster": _s(row.get("ceph_cluster")),
            "phase": _s(row.get("phase")),
            "health_message": _s(row.get("health_message")),
            "port": _s(row.get("port")),
            "tls": _s(row.get("tls")),
            "members": _s(row.get("members")),
        }
        for row in _rows(snap.get("object_stores"))
    ]


def operation_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "id": _s(row.get("id")),
            "kind": _s(row.get("kind")),
            "phase": _s(row.get("phase")),
            "vm": _s(row.get("vm")),
            "source_node": _s(row.get("source_node")),
            "target_node": _s(row.get("target_node")),
            "detail": _s(row.get("detail")),
        }
        for row in _rows(snap.get("operations"))
    ]


def audit_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    return [
        {
            "id": _s(row.get("id")),
            "actor": _s(row.get("actor")),
            "action": _s(row.get("action")),
            "resource": _s(row.get("resource")),
            "created_at": _s(row.get("created_at")),
            "detail": _s(row.get("detail")),
        }
        for row in _rows(snap.get("audit"))
    ]


def node_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    storage = snap.get("storage")
    if not isinstance(storage, dict):
        return []
    rows = []
    for node in _rows(storage.get("nodes")):
        disks = node.get("disks") or []
        rows.append(
            {
                "node_id": _s(node.get("node_id")),
                "hostname": _s(node.get("hostname")),
                "address": _s(node.get("address")),
                "backend": _s(node.get("storage_backend")),
                "luks": _s(node.get("luks_method")),
                "disks": _s(len(disks) if isinstance(disks, list) else 0),
            }
        )
    return rows


def disk_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    storage = snap.get("storage")
    if not isinstance(storage, dict):
        return []
    rows: list[dict[str, str]] = []
    for node in _rows(storage.get("nodes")):
        for disk in _rows(node.get("disks")):
            rows.append(
                {
                    "node": _s(node.get("hostname") or node.get("node_id")),
                    "node_id": _s(node.get("node_id")),
                    "disk": _s(disk.get("name")),
                    "size": _s(disk.get("size")),
                    "role": _s(disk.get("role_hint")),
                    "mount": _s(disk.get("mountpoint")),
                    "fstype": _s(disk.get("fstype")),
                }
            )
    return rows


def compliance_node_rows(snap: dict[str, Any]) -> list[dict[str, str]]:
    compliance = snap.get("compliance")
    if not isinstance(compliance, dict):
        return []
    return [
        {
            "node_id": _s(row.get("node_id")),
            "hostname": _s(row.get("hostname")),
            "address": _s(row.get("address")),
            "approval_status": _s(row.get("approval_status")),
            "cert_expiry_days": _s(row.get("cert_expiry_days")),
            "luks_method": _s(row.get("luks_method")),
        }
        for row in _rows(compliance.get("nodes"))
    ]


def all_rows(snap: dict[str, Any]) -> dict[str, list[dict[str, str]]]:
    return {
        "shown_vms": vm_rows(snap),
        "shown_volumes": volume_rows(snap),
        "shown_networks": network_rows(snap),
        "shown_ifaces": iface_rows(snap),
        "shown_ceph": ceph_rows(snap),
        "shown_filesystems": filesystem_rows(snap),
        "shown_objects": object_rows(snap),
        "shown_operations": operation_rows(snap),
        "shown_audit": audit_rows(snap),
        "shown_nodes": node_rows(snap),
        "shown_disks": disk_rows(snap),
        "shown_compliance_nodes": compliance_node_rows(snap),
    }


def filter_rows(rows: dict[str, list[dict[str, str]]], text: str) -> dict[str, list[dict[str, str]]]:
    tokens = [token for token in text.lower().split() if token]
    if not tokens:
        return rows

    def keep(row: dict[str, str]) -> bool:
        blob = " ".join(row.values()).lower()
        return all(token in blob for token in tokens)

    return {key: [row for row in value if keep(row)] for key, value in rows.items()}


def metrics(snap: dict[str, Any]) -> dict[str, str]:
    compliance = snap.get("compliance") if isinstance(snap.get("compliance"), dict) else {}
    vms = _rows(snap.get("vms"))
    running = sum(1 for row in vms if str(row.get("state", "")).lower() == "running")
    clusters = _rows(snap.get("ceph_clusters"))
    ceph = "—"
    if clusters:
        ceph = _s(clusters[0].get("health_message") or clusters[0].get("phase"), "—")
    replication = snap.get("replication") if isinstance(snap.get("replication"), dict) else {}
    if replication:
        replication_line = (
            f"outbox {_s(replication.get('outbox_size'), '0')}, "
            f"conflicts {_s(replication.get('unresolved_conflicts'), '0')}"
        )
    else:
        replication_line = "—"
    storage = snap.get("storage") if isinstance(snap.get("storage"), dict) else {}
    node_count = compliance.get("total_nodes")
    if node_count is None:
        node_count = len(_rows(storage.get("nodes")))
    vm_count = compliance.get("total_vms")
    if vm_count is None:
        vm_count = len(vms)
    running_count = compliance.get("running_vms")
    if running_count is None:
        running_count = running
    network_count = compliance.get("total_networks")
    if network_count is None:
        network_count = len(_rows(snap.get("networks")))
    return {
        "vms": _s(vm_count, "0"),
        "running": _s(running_count, "0"),
        "volumes": _s(len(_rows(snap.get("volumes"))), "0"),
        "nodes": _s(node_count, "0"),
        "networks": _s(network_count, "0"),
        "ceph": ceph,
        "replication": replication_line,
    }


def _match(rows: list[dict[str, str]], name: str, *keys: str) -> dict[str, str] | None:
    wanted = name.strip().lower()
    if not wanted:
        return None
    for row in rows:
        for key in keys:
            if row.get(key, "").lower() == wanted:
                return row
    return None


def names_of(snap: dict[str, Any]) -> list[tuple[str, str, str]]:
    """(lowercase name, kind, display name) for names an operator might type."""
    found: list[tuple[str, str, str]] = []

    def add(kind: str, display: str) -> None:
        text = display.strip()
        if len(text) < 2:
            return
        found.append((text.lower(), kind, text))

    for row in vm_rows(snap):
        add("vm", row["name"])
        add("vm", row["id"])
    for row in volume_rows(snap):
        add("volume", row["name"])
    for row in network_rows(snap):
        add("network", row["name"])
    for row in ceph_rows(snap):
        add("ceph", row["name"])
    for row in filesystem_rows(snap):
        add("filesystem", row["name"])
    for row in object_rows(snap):
        add("object", row["name"])
    for row in node_rows(snap):
        add("node", row["hostname"])
        add("node", row["node_id"])
    for row in compliance_node_rows(snap):
        add("node", row["hostname"])
        add("node", row["node_id"])
    found.sort(key=lambda item: len(item[0]), reverse=True)
    return found


def describe(snap: dict[str, Any], *, kind: str, name: str, form: str) -> dict[str, str]:
    blank = {
        "title": "",
        "line1": "",
        "line2": "",
        "line3": "",
        "kind": kind,
        "name": name,
        "vm": "",
        "node": "",
    }
    prompts = {
        "vm": ("Virtual machine", "Select a row, or name a VM in the chat."),
        "volume": ("Volume", "Select a volume."),
        "network": ("Network", "Select a network."),
        "ceph": ("Ceph", "Select a cluster, filesystem, or object store."),
        "storage": ("Storage", "Select a node or a disk."),
        "operations": ("Operations", "Filter by VM, phase, or node."),
        "audit": ("Audit", "Search actor, action, or resource."),
        "compliance": ("Compliance", "Select a node, or read the summary below."),
    }
    if not name:
        title, line1 = prompts.get(form, ("", ""))
        blank["title"] = title
        blank["line1"] = line1
        if form == "compliance":
            compliance = snap.get("compliance") if isinstance(snap.get("compliance"), dict) else {}
            if compliance:
                blank["line1"] = (
                    f"{_s(compliance.get('controller_version'))} · {_s(compliance.get('crypto_library'))}"
                )
                blank["line2"] = (
                    f"{_s(compliance.get('approved_nodes'), '0')} approved, "
                    f"{_s(compliance.get('pending_nodes'), '0')} pending, "
                    f"{_s(compliance.get('total_vms'), '0')} VMs"
                )
                suites = compliance.get("tls13_cipher_suites") or []
                if isinstance(suites, list):
                    blank["line3"] = ", ".join(str(item) for item in suites)
        return blank

    if kind == "vm":
        row = _match(vm_rows(snap), name, "name", "id")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No VM by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "vm": row["name"],
                "node": row["node_id"],
                "line1": f"{row['state']} on {row['node_id']}",
                "line2": f"{row['cpu']} vCPU, {row['memory']}, {row['storage_backend']}",
                "line3": row["id"],
            }
        )
        return blank
    if kind == "volume":
        row = _match(volume_rows(snap), name, "name")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No volume by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "vm": row["vm"],
                "node": row["node_id"],
                "line1": f"{row['backend']} {row['size']} · {row['attach_state'] or 'not attached'}",
                "line2": f"pool {row['pool'] or '—'} on {row['node_id'] or '—'}",
                "line3": f"VM {row['vm'] or '—'} · encrypted {row['encrypted']}",
            }
        )
        return blank
    if kind == "network":
        row = _match(network_rows(snap), name, "name")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No network by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "node": row["node_id"],
                "line1": f"{row['network_type']} on {row['node_id']}",
                "line2": f"external {row['external_ip'] or '—'} · gateway {row['gateway_ip'] or '—'}",
                "line3": f"vlan {row['vlan_id'] or '—'}",
            }
        )
        return blank
    if kind == "ceph":
        row = _match(ceph_rows(snap), name, "name")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No Ceph cluster by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "line1": row["health_message"] or row["phase"],
                "line2": f"public {row['public_network'] or '—'} · size {row['size'] or '—'}",
                "line3": row["members"],
            }
        )
        return blank
    if kind == "filesystem":
        row = _match(filesystem_rows(snap), name, "name")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No shared filesystem by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "kind": "filesystem",
                "line1": f"{row['phase']} on {row['ceph_cluster']}",
                "line2": row["health_message"],
                "line3": f"quota {row['quota'] or '—'} · clients {row['clients'] or '0'}",
            }
        )
        return blank
    if kind == "object":
        row = _match(object_rows(snap), name, "name")
        if row is None:
            blank["title"] = name
            blank["line1"] = "No object store by that name."
            return blank
        blank.update(
            {
                "title": row["name"],
                "name": row["name"],
                "kind": "object",
                "line1": f"{row['phase']} on {row['ceph_cluster']}",
                "line2": f"port {row['port'] or '—'} · tls {row['tls']}",
                "line3": row["members"],
            }
        )
        return blank
    if kind == "node":
        row = _match(node_rows(snap), name, "hostname", "node_id") or _match(
            compliance_node_rows(snap), name, "hostname", "node_id"
        )
        if row is None:
            blank["title"] = name
            blank["line1"] = "No node by that name."
            return blank
        blank.update(
            {
                "title": row.get("hostname") or row.get("node_id") or name,
                "name": row.get("node_id") or name,
                "node": row.get("node_id") or "",
                "line1": row.get("address") or row.get("approval_status") or "",
                "line2": row.get("backend") or row.get("luks_method") or "",
                "line3": f"disks {row['disks']}" if "disks" in row else row.get("cert_expiry_days", ""),
            }
        )
        return blank
    blank["title"] = name
    return blank


class SnapshotCache:
    def __init__(self) -> None:
        self._snap: dict[str, Any] | None = None

    def clear(self) -> None:
        self._snap = None

    def get(self, force: bool = False) -> dict[str, Any]:
        if force or self._snap is None:
            self._snap = load_snapshot()
        return self._snap


cache = SnapshotCache()


def load_snapshot() -> dict[str, Any]:
    path = os.environ.get("KCORE_AGENT_SNAPSHOT", "").strip()
    if path:
        try:
            with open(path, encoding="utf-8") as handle:
                return normalize(json.load(handle))
        except (OSError, json.JSONDecodeError) as err:
            return empty_snapshot(f"snapshot file: {err}")

    if os.environ.get("KCORE_AGENT_OFFLINE", "").strip() == "1":
        return empty_snapshot("offline")

    source = os.environ.get("KCORE_AGENT_SOURCE", "kcore").strip().lower()
    if source != "dashboard":
        from .kcore_mcp import load_from_kcore

        return load_from_kcore()

    url = os.environ.get("KCORE_DASHBOARD_SNAPSHOT_URL", "").strip()
    if not url:
        url = "http://127.0.0.1:8080/api/agent-snapshot"
    try:
        with urllib.request.urlopen(url, timeout=2) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except (OSError, urllib.error.URLError, json.JSONDecodeError, TimeoutError) as err:
        return empty_snapshot(f"dashboard snapshot: {err}")
    if not isinstance(payload, dict):
        return empty_snapshot("dashboard snapshot was not an object")
    snap = normalize(payload)
    if snap["source"] == "unavailable":
        snap["source"] = "dashboard"
    return snap
