"""Prefab view: one page, a chat, and a form that follows the section."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from prefab_ui.actions import AppendState, SetState, ShowToast
from prefab_ui.actions.mcp import CallTool, SendMessage
from prefab_ui.app import AppTool, PrefabApp
from prefab_ui.components import (
    Alert,
    AlertDescription,
    Button,
    Card,
    CardContent,
    CardHeader,
    CardTitle,
    Checkbox,
    Column,
    DataTable,
    DataTableColumn,
    ForEach,
    Form,
    Heading,
    If,
    Input,
    Label,
    Metric,
    Muted,
    Row,
    Select,
    SelectOption,
    Text,
    Textarea,
)
from prefab_ui.rx import RESULT, STATE, Rx

from . import service

Handler = Callable[..., dict[str, Any]]

_SECTIONS = (
    ("overview", "Overview"),
    ("vms", "VMs"),
    ("volumes", "Volumes"),
    ("ceph", "Ceph"),
    ("networks", "Networks"),
    ("storage", "Storage"),
    ("operations", "Operations"),
    ("audit", "Audit"),
    ("compliance", "Compliance"),
)


def scene_updates() -> list[SetState]:
    keys = (
        "section",
        "form",
        "focus",
        "focus_kind",
        "filter",
        "notice",
        "reply",
        "power",
        "migrate_to",
        "confirm",
        "selected",
        "warning",
        "source",
        "metrics",
        "audit_query",
        "op_query",
        "shown_vms",
        "shown_volumes",
        "shown_networks",
        "shown_ifaces",
        "shown_ceph",
        "shown_filesystems",
        "shown_objects",
        "shown_operations",
        "shown_audit",
        "shown_nodes",
        "shown_disks",
        "shown_compliance_nodes",
    )
    return [SetState(key, getattr(RESULT, key)) for key in keys]


def _table(columns: list[tuple[str, str]], rows_key: str, on_row_click: Any = None) -> DataTable:
    return DataTable(
        columns=[
            DataTableColumn(key=key, header=header, sortable=True) for key, header in columns
        ],
        rows=Rx(rows_key),
        search=True,
        paginated=True,
        page_size=8,
        on_row_click=on_row_click,
    )


def build_console(state: dict[str, Any], *, transport: str) -> PrefabApp:
    """transport is ``mcp`` inside a host, or ``http`` for the dashboard frame."""

    def call(op: str, arguments: dict[str, Any], tool: Handler, on_success: list[Any]) -> Any:
        if transport == "mcp":
            return CallTool(tool, arguments=arguments, on_success=on_success)
        from prefab_ui.actions import Fetch

        return Fetch.post(
            "/api/turn",
            body={"op": op, **arguments},
            on_success=on_success,
            on_error=ShowToast("{{ $error }}", variant="error"),
        )

    updates = scene_updates()

    def show(section: str) -> Any:
        return call(
            "show",
            {"section": section, "filter": "", "focus": ""},
            service.show_section,
            updates,
        )

    def focus(kind: str, name_expr: str) -> Any:
        return call(
            "focus",
            {"kind": kind, "name": name_expr},
            service.focus_resource,
            updates,
        )

    chat_success = [
        *scene_updates(),
        AppendState("messages", {"role": "console", "text": RESULT.reply}),
        SetState("draft", ""),
    ]
    chat_actions: list[Any] = [
        AppendState("messages", {"role": "you", "text": "{{ draft }}"}),
    ]
    if transport == "mcp":
        chat_actions.append(SendMessage("{{ draft }}"))
    chat_actions.append(
        call(
            "interpret",
            {
                "message": "{{ draft }}",
                "section": "{{ section }}",
                "focus": "{{ focus }}",
                "filter": "{{ filter }}",
            },
            service.interpret_turn,
            chat_success,
        )
    )

    with Column(gap=4, css_class="p-4") as view:
        with Row(gap=3, align="center"):
            Heading("kcore operator")
            Muted("{{ source }}")
            Button(
                "Refresh",
                button_type="button",
                on_click=call(
                    "refresh",
                    {
                        "section": "{{ section }}",
                        "focus": "{{ focus }}",
                        "filter": "{{ filter }}",
                        "form": "{{ form }}",
                    },
                    service.refresh_snapshot,
                    updates,
                ),
            )
        with If("warning != ''"):
            with Alert(variant="warning"):
                AlertDescription("{{ warning }}")
        with Row(gap=3, css_class="flex-wrap"):
            Metric(label="VMs", value="{{ metrics.vms }}")
            Metric(label="Running", value="{{ metrics.running }}")
            Metric(label="Volumes", value="{{ metrics.volumes }}")
            Metric(label="Nodes", value="{{ metrics.nodes }}")
            Metric(label="Networks", value="{{ metrics.networks }}")
            Metric(label="Ceph", value="{{ metrics.ceph }}")
        with Row(gap=2, css_class="flex-wrap"):
            for key, label in _SECTIONS:
                Button(label, button_type="button", on_click=show(key))
        Text("Showing {{ section }}")
        with If("notice != ''"):
            with Alert(variant="info"):
                AlertDescription("{{ notice }}")
        with Row(gap=4, align="start"):
            with Column(gap=4, css_class="flex-1"):
                with If("section == 'overview'"):
                    Text("Replication {{ metrics.replication }}")
                    Heading("Recent operations", level=3)
                    _table(
                        [
                            ("kind", "Kind"),
                            ("phase", "Phase"),
                            ("vm", "VM"),
                            ("source_node", "From"),
                            ("target_node", "To"),
                        ],
                        "shown_operations",
                    )
                    Heading("Recent audit", level=3)
                    _table(
                        [
                            ("created_at", "When"),
                            ("actor", "Actor"),
                            ("action", "Action"),
                            ("resource", "Resource"),
                        ],
                        "shown_audit",
                    )
                with If("section == 'vms'"):
                    _table(
                        [
                            ("name", "Name"),
                            ("state", "State"),
                            ("node_id", "Node"),
                            ("storage_backend", "Storage"),
                            ("cpu", "vCPU"),
                            ("memory", "Memory"),
                        ],
                        "shown_vms",
                        focus("vm", "{{ $event.name }}"),
                    )
                with If("section == 'volumes'"):
                    _table(
                        [
                            ("name", "Name"),
                            ("backend", "Backend"),
                            ("pool", "Pool"),
                            ("node_id", "Node"),
                            ("attach_state", "Attach"),
                            ("size", "Size"),
                            ("vm", "VM"),
                            ("encrypted", "Encrypted"),
                        ],
                        "shown_volumes",
                        focus("volume", "{{ $event.name }}"),
                    )
                with If("section == 'ceph'"):
                    Heading("Clusters", level=3)
                    _table(
                        [
                            ("name", "Name"),
                            ("phase", "Phase"),
                            ("health_message", "Health"),
                            ("public_network", "Public network"),
                            ("members", "Members"),
                        ],
                        "shown_ceph",
                        focus("ceph", "{{ $event.name }}"),
                    )
                    Heading("Shared filesystems", level=3)
                    _table(
                        [
                            ("name", "Name"),
                            ("ceph_cluster", "Cluster"),
                            ("phase", "Phase"),
                            ("quota", "Quota"),
                            ("clients", "Clients"),
                        ],
                        "shown_filesystems",
                        focus("filesystem", "{{ $event.name }}"),
                    )
                    Heading("Object stores", level=3)
                    _table(
                        [
                            ("name", "Name"),
                            ("ceph_cluster", "Cluster"),
                            ("phase", "Phase"),
                            ("port", "Port"),
                            ("tls", "TLS"),
                        ],
                        "shown_objects",
                        focus("object", "{{ $event.name }}"),
                    )
                with If("section == 'networks'"):
                    _table(
                        [
                            ("name", "Name"),
                            ("network_type", "Type"),
                            ("node_id", "Node"),
                            ("external_ip", "External"),
                            ("gateway_ip", "Gateway"),
                            ("vlan_id", "VLAN"),
                        ],
                        "shown_networks",
                        focus("network", "{{ $event.name }}"),
                    )
                    Heading("Host interfaces", level=3)
                    _table(
                        [
                            ("node", "Node"),
                            ("name", "Interface"),
                            ("kind", "Kind"),
                            ("state", "State"),
                            ("addresses", "Addresses"),
                        ],
                        "shown_ifaces",
                    )
                with If("section == 'storage'"):
                    _table(
                        [
                            ("hostname", "Host"),
                            ("node_id", "Node"),
                            ("backend", "Backend"),
                            ("luks", "LUKS"),
                            ("disks", "Disks"),
                        ],
                        "shown_nodes",
                        focus("node", "{{ $event.node_id }}"),
                    )
                    Heading("Disks", level=3)
                    _table(
                        [
                            ("node", "Node"),
                            ("disk", "Disk"),
                            ("size", "Size"),
                            ("role", "Role"),
                            ("mount", "Mount"),
                            ("fstype", "Filesystem"),
                        ],
                        "shown_disks",
                    )
                with If("section == 'operations'"):
                    _table(
                        [
                            ("id", "Id"),
                            ("kind", "Kind"),
                            ("phase", "Phase"),
                            ("vm", "VM"),
                            ("source_node", "From"),
                            ("target_node", "To"),
                            ("detail", "Detail"),
                        ],
                        "shown_operations",
                    )
                with If("section == 'audit'"):
                    _table(
                        [
                            ("created_at", "When"),
                            ("actor", "Actor"),
                            ("action", "Action"),
                            ("resource", "Resource"),
                            ("detail", "Detail"),
                        ],
                        "shown_audit",
                    )
                with If("section == 'compliance'"):
                    _table(
                        [
                            ("hostname", "Host"),
                            ("node_id", "Node"),
                            ("approval_status", "Approval"),
                            ("cert_expiry_days", "Cert days"),
                            ("luks_method", "LUKS"),
                        ],
                        "shown_compliance_nodes",
                        focus("node", "{{ $event.node_id }}"),
                    )
                with If("form != 'none'"):
                    with Card():
                        with CardHeader():
                            CardTitle("{{ selected.title }}")
                        with CardContent():
                            Text("{{ selected.line1 }}")
                            Text("{{ selected.line2 }}")
                            Text("{{ selected.line3 }}")
                            with If("form == 'vm'"):
                                with Form(
                                    on_submit=call(
                                        "stage",
                                        {
                                            "kind": "{{ power }}",
                                            "target": "{{ focus }}",
                                            "argument": "{{ migrate_to }}",
                                            "confirm": "{{ confirm }}",
                                            "section": "{{ section }}",
                                            "focus": "{{ focus }}",
                                            "filter": "{{ filter }}",
                                        },
                                        service.stage_action,
                                        [
                                            *scene_updates(),
                                            AppendState(
                                                "messages",
                                                {"role": "console", "text": RESULT.reply},
                                            ),
                                        ],
                                    )
                                ):
                                    Label("Power")
                                    with Select(
                                        name="power",
                                        placeholder="Leave unchanged",
                                        value="{{ power }}",
                                    ):
                                        SelectOption(value="", label="Leave unchanged")
                                        SelectOption(value="start", label="Start")
                                        SelectOption(value="stop", label="Stop")
                                    Label("Migrate to node")
                                    Input(
                                        name="migrate_to",
                                        value="{{ migrate_to }}",
                                        placeholder="node id",
                                    )
                                    Checkbox(
                                        name="confirm",
                                        label="Confirm — record the kctl command, do not run it",
                                    )
                                    Button("Record command")
                            with If("form == 'volume'"):
                                Button(
                                    "Show attached VM",
                                    button_type="button",
                                    on_click=focus("vm", "{{ selected.vm }}"),
                                )
                            with If("form == 'audit'"):
                                with Form(
                                    on_submit=call(
                                        "show",
                                        {
                                            "section": "audit",
                                            "filter": "{{ audit_query }}",
                                            "focus": "",
                                        },
                                        service.show_section,
                                        updates,
                                    )
                                ):
                                    Label("Search audit")
                                    Input(
                                        name="audit_query",
                                        value="{{ audit_query }}",
                                        placeholder="actor, action, or resource",
                                    )
                                    Button("Search")
                            with If("form == 'operations'"):
                                with Form(
                                    on_submit=call(
                                        "show",
                                        {
                                            "section": "operations",
                                            "filter": "{{ op_query }}",
                                            "focus": "",
                                        },
                                        service.show_section,
                                        updates,
                                    )
                                ):
                                    Label("Filter operations")
                                    Input(
                                        name="op_query",
                                        value="{{ op_query }}",
                                        placeholder="vm, phase, or node",
                                    )
                                    Button("Filter")
            with Column(gap=3, css_class="w-96"):
                Heading("Chat", level=3)
                Muted("Ask for a section, a name, or a start, stop, or migrate.")
                with Column(gap=2, css_class="max-h-96 overflow-auto"):
                    with ForEach("messages"):
                        Text("{{ role }}")
                        Text("{{ text }}")
                with Form(on_submit=chat_actions):
                    Textarea(
                        name="draft",
                        value="{{ draft }}",
                        rows=3,
                        placeholder="running vms, ceph, stop my-vm",
                    )
                    Button("Send")

    state = {
        "messages": [
            {
                "role": "console",
                "text": (
                    "Ask for VMs, volumes, Ceph, networks, storage, operations, "
                    "audit, or compliance. Name a workload to open its form."
                ),
            }
        ],
        "draft": "",
        **state,
    }
    app_tools = None
    if transport == "mcp":
        loose = {"type": "object", "properties": {}, "additionalProperties": True}

        def told() -> dict[str, Any]:
            return {
                "section": STATE.section,
                "form": STATE.form,
                "focus": STATE.focus,
                "notice": STATE.notice,
                "reply": STATE.reply,
            }

        def agent_tool(fn: Handler, description: str, on_success: list[Any]) -> AppTool:
            return AppTool(
                fn,
                name=fn.__name__,
                description=description,
                input_schema=loose,
                on_success=on_success,
                result=told(),
            )

        app_tools = [
            agent_tool(
                service.interpret_turn,
                "Move the open console from an operator sentence. Does not change the cluster.",
                chat_success,
            ),
            agent_tool(
                service.show_section,
                "Switch the open console to a dashboard section.",
                scene_updates(),
            ),
            agent_tool(
                service.focus_resource,
                "Open the form for one named resource.",
                scene_updates(),
            ),
            agent_tool(
                service.refresh_snapshot,
                "Reload dashboard data into the open console.",
                scene_updates(),
            ),
            agent_tool(
                service.stage_action,
                "Record a kctl line on the open form. Does not run it.",
                scene_updates(),
            ),
        ]
    return PrefabApp(view=view, state=state, mode="dark", app_tools=app_tools)
