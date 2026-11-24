#!/usr/bin/env python3
"""Server MCP bohongan untuk tests/mcp.rs (Fase 7).

Berbicara JSON-RPC 2.0 NDJSON di stdio, persis transport MCP stdio:
- initialize            -> hasil handshake minimal
- notifications/*       -> diabaikan (notifikasi memang tanpa jawaban)
- tools/list            -> 1 tool: echo {text}
- tools/call (echo)     -> content text = argumen text apa adanya
- method lain ber-id    -> error JSON-RPC -32601
"""
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        mid = msg.get("id")
        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "echo-bohongan", "version": "0.0.1"},
                },
            })
        elif method == "tools/list":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Kembalikan teks yang diberikan, apa adanya.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }]
                },
            })
        elif method == "tools/call":
            params = msg.get("params") or {}
            args = params.get("arguments") or {}
            if params.get("name") == "echo":
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "content": [{"type": "text", "text": args.get("text", "")}],
                        "isError": False,
                    },
                })
            else:
                send({
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "content": [{"type": "text", "text": "tool tidak dikenal"}],
                        "isError": True,
                    },
                })
        elif mid is not None and method is not None:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32601, "message": f"method tidak dikenal: {method}"},
            })
        # notifikasi (tanpa id) diabaikan


if __name__ == "__main__":
    main()
