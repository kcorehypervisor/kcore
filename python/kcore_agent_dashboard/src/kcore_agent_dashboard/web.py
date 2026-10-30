"""Dashboard frame: the same console, talking HTTP instead of an MCP host."""

from __future__ import annotations

from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse
from starlette.routing import Route

from .console import build_console
from .scene import Scene
from .server import mcp
from .service import dispatch, project
from .snapshot import cache, normalize


async def console_page(_request: Request) -> HTMLResponse:
    state = project(normalize(cache.get()), Scene())
    view = build_console(state, transport="http")
    return HTMLResponse(view.html(renderer_mode="cdn"))


async def turn(request: Request) -> JSONResponse:
    try:
        body = await request.json()
    except Exception:
        return JSONResponse(
            {"reply": "Expected a JSON body.", "notice": "Expected a JSON body."},
            status_code=400,
        )
    if not isinstance(body, dict):
        return JSONResponse(
            {"reply": "Expected a JSON object.", "notice": "Expected a JSON object."},
            status_code=400,
        )
    return JSONResponse(dispatch(body))


async def snapshot_page(_request: Request) -> JSONResponse:
    return JSONResponse(normalize(cache.get()))


def build_asgi():
    asgi = mcp.http_app(path="/mcp")
    asgi.router.routes = [
        Route("/", console_page, methods=["GET"]),
        Route("/api/turn", turn, methods=["POST"]),
        Route("/api/snapshot", snapshot_page, methods=["GET"]),
        *asgi.router.routes,
    ]
    return asgi
