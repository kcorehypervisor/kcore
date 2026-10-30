"""FastMCP server: the operator console, plus generative UI for a custom layout."""

from __future__ import annotations

from fastmcp import FastMCP, FastMCPApp
from fastmcp.apps.generative import GenerativeUI
from fastmcp.tools.base import ToolResult

from . import service
from .console import build_console
from .scene import Scene
from .snapshot import cache, normalize

mcp = FastMCP("kcore operator")
app = FastMCPApp("kcore-operator")


def operator_console(brief: str = "") -> ToolResult:
    """Open the one-page kcore operator console.

    Covers overview, VMs, volumes, Ceph, networks, storage, operations,
    audit, and compliance. Cluster reads go through the kcore MCP server
    (kcore_read and kcore_rpc) and the certificates in ~/.kcore/config.
    Pass brief to open on a matching section or resource, for example
    "running vms" or "ceph". Chat and the app tools show_section,
    focus_resource, and interpret_turn change the form in place. Start,
    stop, and migrate are sent with kcore_operation after the operator
    ticks confirm. Install, drain, delete, and Ceph changes are not
    sent from this console. Use generate_prefab_ui when the operator
    wants a different layout of the same data.
    """
    if brief.strip():
        state = service.interpret_turn(brief)
    else:
        state = service.project(normalize(cache.get()), Scene())
    view = build_console(state, transport="mcp")
    metrics = state["metrics"]
    summary = (
        f"Operator console open on {state['section']}. "
        f"VMs {metrics['vms']}, running {metrics['running']}, "
        f"volumes {metrics['volumes']}, ceph {metrics['ceph']}. "
        f"{state['reply']}"
    )
    return ToolResult(content=summary, structured_content=view)


app.ui()(operator_console)
app.tool(model=True)(service.show_section)
app.tool(model=True)(service.focus_resource)
app.tool(model=True)(service.interpret_turn)
app.tool(model=True)(service.refresh_snapshot)
app.tool(model=True)(service.stage_action)
app.tool(model=True)(service.cluster_summary)
mcp.add_provider(app)
mcp.add_provider(GenerativeUI())
