import json

from kcore_agent_dashboard.console import build_console
from kcore_agent_dashboard.scene import Scene
from kcore_agent_dashboard.service import interpret_turn, project, stage_action
from kcore_agent_dashboard.snapshot import normalize

SNAP = normalize(
    {
        "source": "test",
        "vms": [
            {
                "id": "vm-1",
                "name": "alpha",
                "state": "Running",
                "node_id": "node-a",
                "cpu": 2,
                "memory": "1 GiB",
                "storage_backend": "Ceph",
            }
        ],
        "volumes": [],
        "ceph_clusters": [{"name": "kcore-san", "health_message": "HEALTH_OK"}],
    }
)


def test_turn_opens_vms_and_filters():
    state = project(SNAP, Scene())
    # interpret_turn reads the cache; call the pure function through project instead.
    from kcore_agent_dashboard.scene import interpret

    scene = interpret("running vms", SNAP)
    view = project(SNAP, scene)
    assert view["section"] == "vms"
    assert view["shown_vms"][0]["name"] == "alpha"
    assert state["metrics"]["ceph"] == "HEALTH_OK"


def test_console_json_has_chat_and_sections():
    from prefab_ui.app import ResolvedTool

    def resolver(fn):
        name = fn if isinstance(fn, str) else fn.__name__
        return ResolvedTool(
            name=name,
            local_name=name,
            input_schema={"type": "object", "properties": {}, "additionalProperties": True},
        )

    state = project(SNAP, Scene())
    payload = build_console(state, transport="mcp").to_json(tool_resolver=resolver)
    blob = json.dumps(payload)
    assert "kcore operator" in blob
    assert "Chat" in blob
    assert "Send" in blob
    assert "sendMessage" in blob
    for label in ("VMs", "Volumes", "Ceph", "Audit", "Compliance"):
        assert label in blob


def test_http_console_does_not_send_to_the_host_chat():
    state = project(SNAP, Scene())
    blob = json.dumps(build_console(state, transport="http").to_json())
    assert "sendMessage" not in blob
    assert "/api/turn" in blob


def test_stage_action_sends_only_after_confirm(monkeypatch, tmp_path):
    path = tmp_path / "snap.json"
    path.write_text(json.dumps(SNAP), encoding="utf-8")
    monkeypatch.setenv("KCORE_AGENT_SNAPSHOT", str(path))
    monkeypatch.delenv("KCORE_AGENT_OFFLINE", raising=False)
    sent: list[tuple[str, dict]] = []

    def fake_send(action, spec):
        sent.append((action, spec))
        return {"applied": True}

    monkeypatch.setattr("kcore_agent_dashboard.kcore_mcp.run_operation", fake_send)
    from kcore_agent_dashboard import service

    service.cache.clear()
    waiting = stage_action(kind="stop", target="alpha", confirm=False)
    assert "Tick confirm" in waiting["notice"]
    assert sent == []
    sent_state = stage_action(kind="stop", target="alpha", confirm=True)
    assert "Sent `kctl stop vm alpha`" in sent_state["notice"]
    assert sent == [("set-vm-state", {"name": "alpha", "state": "stopped"})]
    assert sent_state["form"] == "vm"
    assert interpret_turn("ceph")["section"] == "ceph"
