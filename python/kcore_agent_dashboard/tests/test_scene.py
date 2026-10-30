from kcore_agent_dashboard.scene import interpret, stage_notice
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
            },
            {
                "id": "vm-2",
                "name": "beta",
                "state": "Stopped",
                "node_id": "node-b",
                "cpu": 1,
                "memory": "512 MiB",
                "storage_backend": "LVM",
            },
        ],
        "volumes": [{"name": "pgdata", "vm": "alpha", "backend": "Ceph", "size": "10 GiB"}],
        "ceph_clusters": [{"name": "kcore-san", "health_message": "HEALTH_OK", "phase": "Healthy"}],
        "networks": [{"name": "lan", "network_type": "bridge", "node_id": "node-a"}],
    }
)


def test_section_and_filter():
    scene = interpret("show running vms", SNAP)
    assert scene.section == "vms"
    assert scene.form == "vm"
    assert scene.filter == "running"


def test_named_vm_opens_its_form():
    scene = interpret("alpha", SNAP)
    assert scene.section == "vms"
    assert scene.focus == "alpha"
    assert scene.focus_kind == "vm"
    assert scene.form == "vm"


def test_vm_word_does_not_match_vms():
    scene = interpret("show vms", SNAP)
    assert scene.section == "vms"
    assert scene.focus == ""


def test_migrate_fills_the_form_and_does_not_run():
    scene = interpret("migrate alpha to node-b", SNAP)
    assert scene.focus == "alpha"
    assert scene.migrate_to == "node-b"
    assert scene.power == ""
    assert "kctl migrate vm alpha --target-node node-b" in scene.notice
    assert "does not run" in scene.reply


def test_stop_fills_power():
    scene = interpret("stop alpha", SNAP)
    assert scene.power == "stop"
    assert "kctl stop vm alpha" in scene.notice
    assert "does not run" in scene.reply


def test_unknown_keeps_the_current_section():
    scene = interpret("what is the weather", SNAP, section="ceph", focus="kcore-san")
    assert scene.section == "ceph"
    assert scene.focus == "kcore-san"
    assert "I can show" in scene.reply


def test_audit_search():
    scene = interpret("audit CreateVm", SNAP)
    assert scene.section == "audit"
    assert scene.filter == "CreateVm"
    assert scene.form == "audit"


def test_stage_requires_confirm_and_never_applies():
    waiting, power, _migrate = stage_notice("stop", "alpha", "", False)
    assert power == "stop"
    assert "Tick confirm" in waiting
    assert "Recorded" not in waiting

    recorded, power, _migrate = stage_notice("stop", "alpha", "", True)
    assert power == ""
    assert recorded.startswith("Recorded `kctl stop vm alpha`")
    assert "Not sent to the controller" in recorded


def test_stage_rejects_shell_metacharacters():
    notice, _power, _migrate = stage_notice("stop", "alpha; rm", "", True)
    assert "Nothing was recorded" in notice
    assert "Recorded" not in notice
