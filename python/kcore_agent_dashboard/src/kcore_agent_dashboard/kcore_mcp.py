"""Read the cluster through the kcore MCP server.

The console does not open its own gRPC channel. It starts
`kcore-mcp/packages/server/dist/index.js` and calls `kcore_read` and
`kcore_rpc`, the same tools an agent uses. Start, stop, and migrate are
not sent through those tools.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
from pathlib import Path
from typing import Any

from fastmcp import Client
from fastmcp.client.transports import NodeStdioTransport

from .snapshot import empty_snapshot, normalize

_READS = (
    ("kcore_read", {"kind": "vm"}, "listVms"),
    ("kcore_read", {"kind": "volume"}, "listVolumes"),
    ("kcore_read", {"kind": "network"}, "listNetworks"),
    ("kcore_read", {"kind": "ceph-cluster"}, "listCephClusters"),
    ("kcore_read", {"kind": "shared-filesystem"}, "listSharedFilesystems"),
    ("kcore_read", {"kind": "object-store"}, "listObjectStores"),
)

_RPCS = (
    ("controller", "listNodes"),
    ("controller", "getComplianceReport"),
    ("controller", "getNetworkOverview"),
    ("controller", "getStorageOverview"),
    ("controller", "listAuditEvents"),
    ("controller", "listVmOperations"),
    ("controller-admin", "getReplicationStatus"),
    ("controller-admin", "listReplicationConflicts"),
)


def server_script() -> Path:
    override = os.environ.get("KCORE_MCP_SERVER", "").strip()
    if override:
        path = Path(override)
        if not path.is_file():
            raise FileNotFoundError(f"kcore MCP server not found at {path}")
        return path
    for parent in Path(__file__).resolve().parents:
        candidate = parent / "kcore-mcp" / "packages" / "server" / "dist" / "index.js"
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        "kcore MCP server was not found. Set KCORE_MCP_SERVER to packages/server/dist/index.js."
    )


def load_from_kcore() -> dict[str, Any]:
    try:
        calls = _run(_collect)
    except Exception as err:
        return empty_snapshot(f"kcore mcp: {err}")
    snap = snapshot_from_calls(calls)
    snap["source"] = "kcore" if not snap["warnings"] or _any_ok(calls) else "unavailable"
    if snap["source"] == "unavailable" and not snap["warnings"]:
        snap["warnings"] = ["kcore mcp returned no cluster data"]
    return normalize(snap)


def _run(start: Any) -> dict[str, dict[str, Any]]:
    def go() -> dict[str, dict[str, Any]]:
        return asyncio.run(start())

    try:
        asyncio.get_running_loop()
    except RuntimeError:
        return go()
    import concurrent.futures

    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        return pool.submit(go).result()


def snapshot_from_calls(calls: dict[str, dict[str, Any]]) -> dict[str, Any]:
    warnings: list[str] = []
    responses: dict[str, Any] = {}
    for method, payload in calls.items():
        if not payload.get("ok"):
            warnings.append(f"{method}: {payload.get('error') or payload.get('askTheUser') or 'failed'}")
            continue
        responses[method] = payload.get("response") if "response" in payload else payload

    vms = _list(responses.get("listVms"), "vms")
    volumes = _list(responses.get("listVolumes"), "volumes")
    networks = _list(responses.get("listNetworks"), "networks")
    nodes = _list(responses.get("listNodes"), "nodes")
    snap = empty_snapshot("")
    snap["warnings"] = warnings
    snap["source"] = "kcore"
    snap["vms"] = [_vm(row) for row in vms]
    snap["volumes"] = [_volume(row) for row in volumes]
    snap["networks"] = [_snake(row) for row in networks]
    snap["ceph_clusters"] = [_ceph(row) for row in _list(responses.get("listCephClusters"), "cephClusters")]
    snap["shared_filesystems"] = [
        _filesystem(row) for row in _list(responses.get("listSharedFilesystems"), "sharedFilesystems")
    ]
    snap["object_stores"] = [_object_store(row) for row in _list(responses.get("listObjectStores"), "objectStores")]
    snap["operations"] = [_operation(row) for row in _list(responses.get("listVmOperations"), "operations")]
    snap["audit"] = [_snake(row) for row in _list(responses.get("listAuditEvents"), "events")]
    snap["conflicts"] = [_snake(row) for row in _list(responses.get("listReplicationConflicts"), "conflicts")]
    snap["compliance"] = _snake(responses.get("getComplianceReport")) if responses.get("getComplianceReport") else None
    snap["network_overview"] = _network_overview(responses.get("getNetworkOverview"))
    snap["storage"] = _storage(responses.get("getStorageOverview"), nodes)
    snap["replication"] = _snake(responses.get("getReplicationStatus")) if responses.get("getReplicationStatus") else None
    return snap


def _child_env() -> dict[str, str]:
    # The MCP stdio client keeps only HOME, PATH, and a few others unless the
    # transport passes an env. KCORE_CONFIG would otherwise be dropped.
    return dict(os.environ)


def _connection() -> dict[str, Any]:
    timeout = float(os.environ.get("KCORE_MCP_TIMEOUT", "15"))
    connection: dict[str, Any] = {"timeoutMs": int(timeout * 1000)}
    config = os.environ.get("KCORE_CONFIG", "").strip()
    if config:
        connection["config"] = config
    return connection


async def _collect() -> dict[str, dict[str, Any]]:
    script = server_script()
    transport = NodeStdioTransport(
        script,
        node_cmd=os.environ.get("KCORE_MCP_NODE", "node"),
        env=_child_env(),
    )
    timeout = float(os.environ.get("KCORE_MCP_TIMEOUT", "15"))
    connection = _connection()
    async with Client(transport) as client:
        jobs = []
        names = []
        for tool, args, method in _READS:
            names.append(method)
            jobs.append(_call(client, tool, {**args, "connection": connection}, timeout))
        for service, method in _RPCS:
            names.append(method)
            request: dict[str, Any] = {}
            if method == "listAuditEvents":
                request = {"limit": 100}
            elif method == "listVmOperations":
                request = {"includeFinished": True}
            jobs.append(
                _call(
                    client,
                    "kcore_rpc",
                    {
                        "service": service,
                        "method": method,
                        "request": request,
                        "connection": connection,
                    },
                    timeout,
                )
            )
        results = await asyncio.gather(*jobs)
    return dict(zip(names, results, strict=True))


async def _call(client: Client, tool: str, arguments: dict[str, Any], timeout: float) -> dict[str, Any]:
    try:
        result = await client.call_tool_mcp(tool, arguments, timeout=timeout)
    except Exception as err:
        return {"ok": False, "error": str(err)}
    return _payload(result)


def _payload(result: Any) -> dict[str, Any]:
    failed = getattr(result, "is_error", None)
    if failed is None:
        failed = getattr(result, "isError", False)
    if failed:
        return {"ok": False, "error": _text(result) or "kcore mcp tool error"}
    text = _text(result)
    if not text:
        return {"ok": False, "error": "empty kcore mcp response"}
    try:
        parsed = json.loads(text)
    except json.JSONDecodeError as err:
        return {"ok": False, "error": f"kcore mcp returned non-json: {err}"}
    if not isinstance(parsed, dict):
        return {"ok": False, "error": "kcore mcp returned a non-object"}
    return parsed


def _text(result: Any) -> str:
    blocks = getattr(result, "content", None) or []
    parts = []
    for block in blocks:
        text = getattr(block, "text", None)
        if text:
            parts.append(text)
    return "\n".join(parts)


def _any_ok(calls: dict[str, dict[str, Any]]) -> bool:
    return any(payload.get("ok") for payload in calls.values())


def _list(response: Any, key: str) -> list[dict[str, Any]]:
    if not isinstance(response, dict):
        return []
    rows = response.get(key) or []
    if not isinstance(rows, list):
        return []
    return [row for row in rows if isinstance(row, dict)]


def _snake_name(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def _snake(value: Any) -> Any:
    if isinstance(value, list):
        return [_snake(item) for item in value]
    if isinstance(value, dict):
        return {_snake_name(str(key)): _snake(item) for key, item in value.items()}
    return value


def _vm(row: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": row.get("id") or "",
        "name": row.get("name") or "",
        "state": _vm_state(row.get("state")),
        "cpu": row.get("cpu") or 0,
        "memory": _memory(row.get("memoryBytes")),
        "node_id": row.get("nodeId") or "",
        "storage_backend": _backend(row.get("storageBackend")),
    }


def _volume(row: dict[str, Any]) -> dict[str, Any]:
    name = row.get("name") or row.get("vmName") or row.get("vmId") or ""
    return {
        "name": name,
        "backend": _backend(row.get("storageBackend")),
        "pool": row.get("pool") or "",
        "node_id": row.get("nodeId") or "",
        "role": row.get("role") or "",
        "attach_state": row.get("attachState") or "",
        "size": _bytes(row.get("storageSizeBytes")),
        "vm": row.get("vmName") or row.get("vmId") or "",
        "encrypted": "yes" if row.get("encrypted") else "no",
    }


def _ceph(row: dict[str, Any]) -> dict[str, Any]:
    spec = row.get("spec") if isinstance(row.get("spec"), dict) else {}
    status = row.get("status") if isinstance(row.get("status"), dict) else {}
    members: list[str] = []
    devices: list[str] = []
    for node in spec.get("nodes") or []:
        if not isinstance(node, dict):
            continue
        node_id = str(node.get("nodeId") or "")
        if node_id:
            members.append(node_id)
        device = str(node.get("osdDevice") or "")
        if device:
            devices.append(device)
        for extra in node.get("osdDevices") or []:
            if extra:
                devices.append(str(extra))
    return {
        "name": row.get("name") or "",
        "phase": _phase(status.get("phase")),
        "health_message": status.get("healthMessage") or "",
        "public_network": spec.get("publicNetwork") or "",
        "members": ", ".join(members) if members else "—",
        "size": spec.get("size") if spec.get("size") not in (None, "") else "",
        "osd_devices": ", ".join(devices) if devices else "—",
    }


def _filesystem(row: dict[str, Any]) -> dict[str, Any]:
    spec = row.get("spec") if isinstance(row.get("spec"), dict) else {}
    status = row.get("status") if isinstance(row.get("status"), dict) else {}
    clients = spec.get("clients") or []
    quota = spec.get("quotaBytes") or 0
    return {
        "name": row.get("name") or "",
        "ceph_cluster": spec.get("cephCluster") or "",
        "phase": _phase(status.get("phase")),
        "health_message": status.get("healthMessage") or "",
        "quota": "—" if not quota else _bytes(quota),
        "clients": str(len(clients) if isinstance(clients, list) else 0),
    }


def _object_store(row: dict[str, Any]) -> dict[str, Any]:
    spec = row.get("spec") if isinstance(row.get("spec"), dict) else {}
    status = row.get("status") if isinstance(row.get("status"), dict) else {}
    members = spec.get("members") or []
    if not isinstance(members, list):
        members = [members]
    return {
        "name": row.get("name") or "",
        "ceph_cluster": spec.get("cephCluster") or "",
        "phase": _phase(status.get("phase")),
        "health_message": status.get("healthMessage") or "",
        "port": spec.get("port") if spec.get("port") not in (None, "") else "",
        "tls": "yes" if spec.get("tls") else "no",
        "members": ", ".join(str(item) for item in members if item) or "—",
    }


def _operation(row: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": row.get("id") or "",
        "kind": row.get("kind") or "",
        "phase": row.get("phase") or "",
        "vm": row.get("vmId") or "",
        "source_node": row.get("sourceNode") or "",
        "target_node": row.get("targetNode") or "",
        "detail": row.get("detailJson") or "",
    }


def _network_overview(response: Any) -> dict[str, Any] | None:
    if not isinstance(response, dict):
        return None
    overview = _snake(response)
    if not isinstance(overview, dict):
        return None
    for node in overview.get("nodes") or []:
        if not isinstance(node, dict):
            continue
        for iface in node.get("interfaces") or []:
            if isinstance(iface, dict) and not iface.get("kind"):
                iface["kind"] = _iface_kind(str(iface.get("name") or ""))
    return overview


def _storage(response: Any, nodes: list[dict[str, Any]]) -> dict[str, Any] | None:
    storage = _snake(response) if isinstance(response, dict) else None
    if isinstance(storage, dict):
        for node in storage.get("nodes") or []:
            if isinstance(node, dict):
                node["storage_backend"] = _backend(node.get("storage_backend"))
        if storage.get("nodes"):
            return storage
    if not nodes:
        return storage
    return {
        "nodes": [
            {
                "node_id": row.get("nodeId") or "",
                "hostname": row.get("hostname") or "",
                "address": row.get("address") or "",
                "storage_backend": _backend(row.get("storageBackend")),
                "luks_method": row.get("luksMethod") or "",
                "disks": [],
            }
            for row in nodes
        ]
    }


def _vm_state(value: Any) -> str:
    text = str(value or "")
    if "RUNNING" in text or text == "2":
        return "Running"
    if "STOPPED" in text or text == "1":
        return "Stopped"
    if "PAUSED" in text or text == "3":
        return "Paused"
    if "ERROR" in text or text == "4":
        return "Error"
    return "Unknown"


def _phase(value: Any) -> str:
    text = str(value or "")
    for needle, label in (
        ("PENDING", "Pending"),
        ("BOOTSTRAPPING", "Bootstrapping"),
        ("HEALTHY", "Healthy"),
        ("DEGRADED", "Degraded"),
        ("FAILED", "Failed"),
    ):
        if needle in text:
            return label
    return {"1": "Pending", "2": "Bootstrapping", "3": "Healthy", "4": "Degraded", "5": "Failed"}.get(
        text, "Unspecified"
    )


def _backend(value: Any) -> str:
    text = str(value or "").lower()
    if "ceph" in text or text == "4":
        return "Ceph"
    if "zfs" in text or text == "3":
        return "ZFS"
    if "lvm" in text or text == "2":
        return "LVM"
    if "filesystem" in text or text == "1":
        return "Filesystem"
    if not text or text in {"0", "unspecified", "storage_backend_type_unspecified"}:
        return "—"
    return str(value)


def _memory(value: Any) -> str:
    try:
        amount = int(value)
    except (TypeError, ValueError):
        return "0 MiB"
    if amount <= 0:
        return "0 MiB"
    return f"{amount / (1024 * 1024):.0f} MiB"


def _bytes(value: Any) -> str:
    try:
        amount = int(value)
    except (TypeError, ValueError):
        return ""
    if amount <= 0:
        return "0"
    size = float(amount)
    if size >= 1024**4:
        return f"{size / 1024**4:.1f} TiB"
    if size >= 1024**3:
        return f"{size / 1024**3:.1f} GiB"
    if size >= 1024**2:
        return f"{size / 1024**2:.0f} MiB"
    return f"{size / 1024:.0f} KiB"


def _iface_kind(name: str) -> str:
    if name == "lo":
        return "loopback"
    if name.startswith(("br-", "kbr-", "virbr")) or name == "br0":
        return "bridge"
    if name.startswith(("tap-", "vnet")):
        return "tap"
    if "." in name or name.startswith("vlan"):
        return "vlan"
    if name.startswith(("vxlan", "kvx-")):
        return "vxlan"
    if name.startswith(("eno", "eth", "enp", "ens")):
        return "physical"
    if name.startswith("bond"):
        return "bond"
    if name.startswith(("docker", "veth")):
        return "container"
    return "other"
