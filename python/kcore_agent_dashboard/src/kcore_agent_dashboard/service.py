"""Backend tools the console and the agent both call. None of them change the cluster."""

from __future__ import annotations

from typing import Any

from .scene import (
    Scene,
    as_bool,
    form_for,
    interpret,
    normalize_section,
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
    scene = Scene(
        section=section_name,
        form=form or form_for(section_name),
        focus=focus,
        focus_kind=_kind_for_form(form or form_for(section_name)) if focus else "",
        filter=filter,
        reply="Refreshed from the dashboard.",
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
    """Record a kctl start, stop, or migrate line.

    Does not start, stop, or migrate anything. confirm must be true, and
    only after the operator asked to record that command. The cluster is
    unchanged either way.
    """
    notice, power, migrate_to = stage_notice(kind, target or focus, argument, as_bool(confirm))
    section_name = normalize_section(section or "vms")
    scene = Scene(
        section=section_name,
        form="vm",
        focus=target or focus,
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
