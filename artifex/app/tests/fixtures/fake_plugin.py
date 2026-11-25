#!/usr/bin/env python3
# An Artifex plugin for the engine tests, speaking LSP on stdio (the test's
# link carries it over editor_ext parts): it marks lines with TODO, offers
# one completion, and a fix that takes the TODO out.
import json, sys

docs = {}

def read():
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        k, _, v = line.partition(b":")
        if k.lower() == b"content-length":
            n = int(v)
    return json.loads(sys.stdin.buffer.read(n))

def send(msg):
    b = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    sys.stdout.buffer.flush()

def todos(text):
    out = []
    for i, l in enumerate(text.split("\n")):
        c = l.find("TODO")
        if c >= 0:
            out.append((i, c))
    return out

def publish(uri):
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": [
        {"range": {"start": {"line": i, "character": c}, "end": {"line": i, "character": c + 4}}, "severity": 2, "message": "TODO left here"}
        for i, c in todos(docs[uri])]}})

while True:
    m = read()
    meth, p = m.get("method"), m.get("params") or {}
    if meth == "exit":
        sys.exit(0)
    if meth == "textDocument/didOpen":
        d = p["textDocument"]
        docs[d["uri"]] = d["text"]
        publish(d["uri"])
    elif meth == "textDocument/didChange":
        docs[p["textDocument"]["uri"]] = p["contentChanges"][-1]["text"]
        publish(p["textDocument"]["uri"])
    if "id" not in m or meth is None:
        continue
    result = None
    if meth == "initialize":
        result = {"capabilities": {"completionProvider": {}}, "serverInfo": {"name": "todo"}}
    elif meth == "textDocument/completion":
        result = {"isIncomplete": False, "items": [{"label": "prune_todo", "detail": "todo", "sortText": "c"}]}
    elif meth == "textDocument/codeAction":
        uri = p["textDocument"]["uri"]
        line = p["range"]["start"]["line"]
        result = [{"title": "Take the TODO out",
                   "edit": {"changes": {uri: [{"range": {"start": {"line": i, "character": c}, "end": {"line": i, "character": c + 4}}, "newText": "DONE"}]}}}
                  for i, c in todos(docs[uri]) if i == line]
    send({"jsonrpc": "2.0", "id": m["id"], "result": result})
