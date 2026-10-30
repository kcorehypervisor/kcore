"""Run the operator console.

stdio is for an MCP host. The default is the page the dashboard agent
frame loads, with the MCP endpoint on /mcp of the same port.
"""

from __future__ import annotations

import argparse


def main() -> None:
    parser = argparse.ArgumentParser(description="kcore operator console")
    parser.add_argument("--stdio", action="store_true", help="speak MCP on stdin")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8091)
    args = parser.parse_args()

    from .server import mcp

    if args.stdio:
        mcp.run(transport="stdio")
        return

    import uvicorn

    from .web import build_asgi

    uvicorn.run(build_asgi(), host=args.host, port=args.port)


if __name__ == "__main__":
    main()
