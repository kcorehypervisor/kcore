import asyncio

from fastmcp import Client

from kcore_agent_dashboard.server import mcp
from kcore_agent_dashboard.web import build_asgi


def test_agent_tools_include_the_console_and_generative_ui(monkeypatch):
    monkeypatch.setattr(
        "kcore_agent_dashboard.kcore_mcp.run_operation",
        lambda action, spec: {"applied": True, "action": action, "spec": spec},
    )
    async def inner():
        async with Client(mcp) as client:
            names = {tool.name for tool in await client.list_tools()}
            assert "operator_console" in names
            assert "show_section" in names
            assert "focus_resource" in names
            assert "interpret_turn" in names
            assert "stage_action" in names
            assert "generate_prefab_ui" in names
            opened = await client.call_tool("operator_console", {"brief": "volumes"})
            text = opened.content[0].text
            assert "volumes" in text.lower()

            staged = await client.call_tool(
                "stage_action",
                {"kind": "stop", "target": "alpha", "confirm": True},
            )
            staged_text = staged.content[0].text
            assert "Sent `kctl stop vm alpha`" in staged_text

    asyncio.run(inner())


def test_dashboard_frame_routes_share_the_mcp_port():
    paths = {getattr(route, "path", None) for route in build_asgi().router.routes}
    assert "/" in paths
    assert "/api/turn" in paths
    assert "/api/snapshot" in paths
    assert "/mcp" in paths
