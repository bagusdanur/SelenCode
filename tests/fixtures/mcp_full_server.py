#!/usr/bin/env python3
"""Fixture server MCP lengkap (Fase 7): tools + resources + prompts + ping + crash simulation."""
import json
import sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    req_id = msg.get("id")
    params = msg.get("params") or {}

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "serverInfo": {"name": "selen-full-fixture", "version": "1.0.0"},
                "capabilities": {
                    "tools": {},
                    "resources": {},
                    "prompts": {}
                }
            }
        })
    elif method == "notifications/initialized":
        continue
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    {
                        "name": "greet",
                        "description": "Sapa nama tertentu",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "count": {"type": "integer"}
                            },
                            "required": ["name"]
                        }
                    },
                    {
                        "name": "die_once",
                        "description": "Keluar mendadak untuk uji reconnect",
                        "inputSchema": {"type": "object", "properties": {}}
                    }
                ]
            }
        })
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "die_once":
            sys.exit(0)
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": f"Halo {args.get('name', '')}!"}]
            }
        })
    elif method == "resources/list":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "resources": [
                    {
                        "uri": "file:///docs/arch.md",
                        "name": "Architecture Doc",
                        "description": "Ringkasan arsitektur",
                        "mimeType": "text/markdown"
                    }
                ]
            }
        })
    elif method == "resources/read":
        uri = params.get("uri", "")
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "contents": [
                    {"uri": uri, "mimeType": "text/markdown", "text": "# Arsitektur SelenCode"}
                ]
            }
        })
    elif method == "prompts/list":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "prompts": [
                    {
                        "name": "code_review",
                        "description": "Template review kode",
                        "arguments": [{"name": "target", "description": "File target", "required": True}]
                    }
                ]
            }
        })
    elif method == "prompts/get":
        target = (params.get("arguments") or {}).get("target", "main.rs")
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "description": "Review kode",
                "messages": [
                    {"role": "user", "content": {"type": "text", "text": f"Tinjau keamanan {target}"}}
                ]
            }
        })
    elif req_id is not None:
        send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "not found"}})
