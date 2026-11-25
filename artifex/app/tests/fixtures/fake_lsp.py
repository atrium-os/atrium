#!/usr/bin/env python3
# A language server for Artifex's engine tests: it answers completion,
# signature help, references, rename, code actions and formatting from the
# text it was sent, the same way every time.
import json, re, sys

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

def word_at(text, line, col):
    l = text.split("\n")[line]
    s = col
    while s > 0 and (l[s - 1].isalnum() or l[s - 1] == "_"):
        s -= 1
    e = col
    while e < len(l) and (l[e].isalnum() or l[e] == "_"):
        e += 1
    return l[s:e]

def uses(text, word):
    out = []
    for i, l in enumerate(text.split("\n")):
        for m in re.finditer(r"\b%s\b" % re.escape(word), l):
            out.append({"start": {"line": i, "character": m.start()}, "end": {"line": i, "character": m.end()}})
    return out

def answer(m):
    meth, p = m.get("method"), m.get("params") or {}
    uri = p.get("textDocument", {}).get("uri")
    pos = p.get("position", {})
    if meth == "initialize":
        return {"capabilities": {"completionProvider": {"triggerCharacters": [".", "("]},
                                 "signatureHelpProvider": {"triggerCharacters": ["(", ","]}},
                "serverInfo": {"name": "fake"}}
    if meth == "textDocument/completion":
        items = [{"label": "print", "detail": "builtin", "sortText": "a"},
                 {"label": "printf_ish", "insertText": "printf_ish(${1:x})$0", "insertTextFormat": 2, "sortText": "b"},
                 {"label": "open", "sortText": "c"}]
        return {"isIncomplete": False, "items": items}
    if meth == "textDocument/signatureHelp":
        return {"signatures": [{"label": "print(*values, sep=' ')", "parameters": [{"label": [6, 13]}]}], "activeParameter": 0}
    if meth == "textDocument/references":
        w = word_at(docs[uri], pos["line"], pos["character"])
        return [{"uri": uri, "range": r} for r in uses(docs[uri], w)]
    if meth == "textDocument/rename":
        w = word_at(docs[uri], pos["line"], pos["character"])
        return {"changes": {uri: [{"range": r, "newText": p["newName"]} for r in uses(docs[uri], w)]}}
    if meth == "textDocument/codeAction":
        return [{"title": "Add a comment", "isPreferred": True,
                 "edit": {"changes": {uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "# fixed\n"}]}}}]
    if meth == "textDocument/formatting":
        out = []
        for i, l in enumerate(docs[uri].split("\n")):
            n = len(l) - len(l.lstrip("\t"))
            if n:
                out.append({"range": {"start": {"line": i, "character": 0}, "end": {"line": i, "character": n}}, "newText": "    " * n})
        return out
    return None

while True:
    m = read()
    meth = m.get("method")
    if meth == "exit":
        sys.exit(0)
    if meth == "textDocument/didOpen":
        d = m["params"]["textDocument"]
        docs[d["uri"]] = d["text"]
    elif meth == "textDocument/didChange":
        docs[m["params"]["textDocument"]["uri"]] = m["params"]["contentChanges"][-1]["text"]
    if "id" in m and meth is not None:
        send({"jsonrpc": "2.0", "id": m["id"], "result": answer(m)})
