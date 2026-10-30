"""Backend tools the console and a host agent both call.

Reads do not change the cluster. stage_action sends start, stop, or
migrate only after confirm.
"""

from __future__ import annotations

from typing import Any

from .scene import (
    Scene,
    as_bool,
    form_for,
    interpret,
    normalize_section,
    operation_call,
    recorded_command,
    scene_from_focus,
    scene_from_section,
    stage_notice,
)
from .snapshot import all_rows, cache, describe, filter_rows, metrics, normalize


def project(snap: dict[str, Any], scene: Scene) -> dict[str, Any]:
    rows = filter_rows(all_rows(snap), scene.filter)
    selected = describe(
        snap,
        kind=scene.focus_kind,
        name=scene.focus,
        form=scene.form,
    )
    warning = " ".join(str(item) for item in snap.get("warnings") or [])
    return {
        "section": scene.section,
        "form": scene.form,
        "focus": scene.focus,
        "focus_kind": scene.focus_kind,
        "filter": scene.filter,
        "notice": scene.notice,
        "reply": scene.reply,
        "power": scene.power,
        "migrate_to": scene.migrate_to,
        "confirm": False,
        "selected": selected,
        "warning": warning,
        "source": str(snap.get("source") or "unavailable"),
        "metrics": metrics(snap),
        "audit_query": scene.filter if scene.section == "audit" else "",
        "op_query": scene.filter if scene.section == "operations" else "",
        **rows,
    }


def _snap() -> dict[str, Any]:
    return normalize(cache.get())


def show_section(section: str, filter: str = "", focus: str = "") -> dict[str, Any]:
    """Switch the open console to one section and optional filter.

    Sections: overview, vms, volumes, ceph, networks, storage, operations,
    audit, compliance. This only changes the page.
    """
    scene = scene_from_section(section, filter_text=filter, focus=focus)
    if focus:
        scene.focus_kind = _kind_for_form(scene.form)
    return project(_snap(), scene)


def focus_resource(kind: str, name: str) -> dict[str, Any]:
    """Open the form for one VM, volume, network, Ceph object, or node.

    kind is vm, volume, network, ceph, filesystem, object, or node.
    This only changes the form.
    """
    return project(_snap(), scene_from_focus(_snap(), kind, name))


def interpret_turn(
    message: str,
    section: str = "overview",
    focus: str = "",
    filter: str = "",
) -> dict[str, Any]:
    """Read an operator sentence and move the console.

    Examples: "running vms", "ceph", "audit CreateVm", "stop demo",
    "migrate demo to node-b". Start, stop, and migrate only fill the form.
    """
    text = message.strip().lower()
    if text in {"refresh", "reload"}:
        return refresh_snapshot(section=section, focus=focus, filter=filter, form=form_for(section))
    snap = _snap()
    scene = interpret(message, snap, section=section, focus=focus, filter_text=filter)
    return project(snap, scene)


def refresh_snapshot(
    section: str = "overview",
    focus: str = "",
    filter: str = "",
    form: str = "",
) -> dict[str, Any]:
    """Reload the dashboard snapshot and keep the current section."""
    snap = normalize(cache.get(force=True))
    section_name = normalize_section(section)
    if snap.get("source") == "kcore":
        reply = "Refreshed from kcore."
    elif snap.get("source") == "dashboard":
        reply = "Refreshed from the dashboard."
    else:
        warnings = snap.get("warnings") or ["Refresh failed."]
        reply = str(warnings[0])
    scene = Scene(
        section=section_name,
        form=form or form_for(section_name),
        focus=focus,
        focus_kind=_kind_for_form(form or form_for(section_name)) if focus else "",
        filter=filter,
        reply=reply,
    )
    return project(snap, scene)


def stage_action(
    kind: str = "",
    target: str = "",
    argument: str = "",
    confirm: bool = False,
    section: str = "vms",
    focus: str = "",
    filter: str = "",
) -> dict[str, Any]:
    """Start, stop, or migrate one VM through kcore after confirm.

    Without confirm the cluster is unchanged. Install, drain, delete, and
    Ceph changes are not available here.
    """
    vm_name = target or focus
    confirmed = as_bool(confirm)
    notice, power, migrate_to = stage_notice(kind, vm_name, argument, confirmed)
    if confirmed and notice.startswith("Sending `"):
        call = operation_call(kind, vm_name, argument)
        command = recorded_command(kind, vm_name, argument) or ""
        if call is None:
            notice = "Nothing was sent."
        else:
            from .kcore_mcp import run_operation

            action, spec = call
            result = run_operation(action, spec)
            if result.get("applied") is True and result.get("ok") is not False:
                notice = f"Sent `{command}`. The controller accepted it."
            else:
                reason = result.get("error") or result.get("askTheUser") or "the controller did not apply it"
                notice = f"Did not apply `{command}`. {reason}"
        power = ""
        migrate_to = ""
    section_name = normalize_section(section or "vms")
    scene = Scene(
        section=section_name,
        form="vm",
        focus=vm_name,
        focus_kind="vm",
        filter=filter,
        notice=notice,
        reply=notice,
        power=power,
        migrate_to=migrate_to,
    )
    return project(_snap(), scene)


def cluster_summary() -> dict[str, Any]:
    """Counts and names the operator console can show. Read-only."""
    snap = _snap()
    rows = all_rows(snap)
    return {
        "source": snap.get("source"),
        "warnings": snap.get("warnings") or [],
        "metrics": metrics(snap),
        "vms": [row["name"] for row in rows["shown_vms"]],
        "volumes": [row["name"] for row in rows["shown_volumes"]],
        "ceph_clusters": [row["name"] for row in rows["shown_ceph"]],
        "networks": [row["name"] for row in rows["shown_networks"]],
        "nodes": [row["hostname"] or row["node_id"] for row in rows["shown_nodes"]],
    }


def dispatch(body: dict[str, Any]) -> dict[str, Any]:
    op = str(body.get("op") or "interpret")
    if op == "show":
        return show_section(
            str(body.get("section") or "overview"),
            str(body.get("filter") or ""),
            str(body.get("focus") or ""),
        )
    if op == "focus":
        return focus_resource(str(body.get("kind") or ""), str(body.get("name") or ""))
    if op == "refresh":
        return refresh_snapshot(
            section=str(body.get("section") or "overview"),
            focus=str(body.get("focus") or ""),
            filter=str(body.get("filter") or ""),
            form=str(body.get("form") or ""),
        )
    if op == "stage":
        return stage_action(
            kind=str(body.get("kind") or ""),
            target=str(body.get("target") or ""),
            argument=str(body.get("argument") or ""),
            confirm=as_bool(body.get("confirm", False)),
            section=str(body.get("section") or "vms"),
            focus=str(body.get("focus") or ""),
            filter=str(body.get("filter") or ""),
        )
    return interpret_turn(
        str(body.get("message") or ""),
        section=str(body.get("section") or "overview"),
        focus=str(body.get("focus") or ""),
        filter=str(body.get("filter") or ""),
    )


def _kind_for_form(form: str) -> str:
    return {
        "vm": "vm",
        "volume": "volume",
        "network": "network",
        "ceph": "ceph",
        "storage": "node",
        "compliance": "node",
    }.get(form, "")
