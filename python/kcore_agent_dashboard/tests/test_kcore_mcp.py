import asyncio

from kcore_agent_dashboard.kcore_mcp import server_script, snapshot_from_calls
from kcore_agent_dashboard.snapshot import metrics, vm_rows


def test_kcore_reads_become_console_rows():
    snap = snapshot_from_calls(
        {
            "listVms": {
                "ok": True,
                "response": {
                    "vms": [
                        {
                            "id": "vm-1",
                            "name": "web",
                            "state": "VM_STATE_RUNNING",
                            "cpu": 2,
                            "memoryBytes": "2147483648",
                            "nodeId": "node-a",
                            "storageBackend": "ceph",
                        }
                    ]
                },
            },
            "listVolumes": {
                "ok": True,
                "response": {
                    "volumes": [
                        {
                            "name": "data",
                            "vmName": "web",
                            "nodeId": "node-a",
                            "storageBackend": "ceph",
                            "storageSizeBytes": "10737418240",
                            "pool": "kcore-vms",
                            "role": "data",
                            "attachState": "attached",
                            "encrypted": True,
                        }
                    ]
                },
            },
            "listCephClusters": {
                "ok": True,
                "response": {
                    "cephClusters": [
                        {
                            "name": "lab",
                            "spec": {
                                "publicNetwork": "192.168.47.0/24",
                                "size": 3,
                                "nodes": [{"nodeId": "node-a", "osdDevice": "/dev/sdb"}],
                            },
                            "status": {"phase": "CEPH_CLUSTER_PHASE_HEALTHY", "healthMessage": "HEALTH_OK"},
                        }
                    ]
                },
            },
            "listVmOperations": {
                "ok": True,
                "response": {
                    "operations": [
                        {
                            "id": "op-1",
                            "vmId": "web",
                            "kind": "migrate",
                            "phase": "done",
                            "sourceNode": "node-a",
                            "targetNode": "node-b",
                            "detailJson": "moved",
                        }
                    ]
                },
            },
            "getComplianceReport": {
                "ok": True,
                "response": {"totalNodes": 2, "totalVms": 1, "runningVms": 1, "controllerVersion": "0.4.2"},
            },
            "listSharedFilesystems": {"ok": False, "error": "unavailable"},
        }
    )
    assert snap["source"] == "kcore"
    assert snap["warnings"] == ["listSharedFilesystems: unavailable"]
    assert vm_rows(snap)[0]["state"] == "Running"
    assert vm_rows(snap)[0]["memory"] == "2048 MiB"
    assert snap["volumes"][0]["name"] == "data"
    assert snap["volumes"][0]["size"] == "10.0 GiB"
    assert snap["ceph_clusters"][0]["phase"] == "Healthy"
    assert snap["ceph_clusters"][0]["members"] == "node-a"
    assert snap["operations"][0]["vm"] == "web"
    assert metrics(snap)["running"] == "1"
    assert snap["compliance"]["controller_version"] == "0.4.2"


def test_load_from_kcore_can_run_inside_the_console_event_loop(monkeypatch):
    async def fake_collect():
        return {"listVms": {"ok": True, "response": {"vms": [{"id": "vm-1", "name": "web", "state": "VM_STATE_STOPPED"}]}}}

    monkeypatch.setattr("kcore_agent_dashboard.kcore_mcp._collect", fake_collect)

    async def inside_loop():
        from kcore_agent_dashboard.kcore_mcp import load_from_kcore

        return load_from_kcore()

    snap = asyncio.run(inside_loop())
    assert snap["source"] == "kcore"
    assert snap["vms"][0]["name"] == "web"


def test_connection_forwards_kcore_config(monkeypatch):
    monkeypatch.setenv("KCORE_CONFIG", "/tmp/kcore-config.yaml")
    monkeypatch.setenv("KCORE_MCP_TIMEOUT", "9")
    from kcore_agent_dashboard.kcore_mcp import _connection

    connection = _connection()
    assert connection["config"] == "/tmp/kcore-config.yaml"
    assert connection["timeoutMs"] == 9000


def test_connection_uses_the_default_context_file(monkeypatch):
    monkeypatch.delenv("KCORE_CONFIG", raising=False)
    from kcore_agent_dashboard.kcore_mcp import _connection

    assert "config" not in _connection()


def test_server_script_points_at_the_kcore_mcp_build():
    script = server_script()
    assert script.name == "index.js"
    assert script.parts[-4:] == ("packages", "server", "dist", "index.js")
