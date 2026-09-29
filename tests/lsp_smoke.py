#!/usr/bin/env python3
"""Run after cargo build: python3 tests/lsp_smoke.py target/release/scope."""
import json
import pathlib
import queue
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading

binary = str(pathlib.Path(sys.argv[1]).resolve())

with tempfile.TemporaryDirectory(prefix="scope-lsp-") as tmp:
    root = pathlib.Path(tmp).resolve()
    source = root / "a #é.rs"
    source.write_text("/* 🦀é */ fn café() {}\n", encoding="utf-8")
    process = subprocess.Popen(
        [binary, "lsp", str(root)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    replies = queue.Queue()
    database = None
    lock = None

    def reader():
        try:
            while True:
                headers = {}
                while True:
                    row = process.stdout.readline()
                    if not row:
                        raise EOFError("server stdout closed")
                    if row == b"\r\n":
                        break
                    key, value = row.decode().split(":", 1)
                    headers[key.lower()] = value.strip()
                replies.put(json.loads(
                    process.stdout.read(int(headers["content-length"]))
                ))
        except Exception as error:
            replies.put(error)

    threading.Thread(target=reader, daemon=True).start()

    def send(method, params=None, id=None):
        value = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            value["params"] = params
        if id is not None:
            value["id"] = id
        body = json.dumps(value, ensure_ascii=False).encode()
        process.stdin.write(
            f"Content-Length: {len(body)}\r\n\r\n".encode() + body
        )
        process.stdin.flush()

    def receive(id=None, timeout=15):
        value = replies.get(timeout=timeout)
        if isinstance(value, Exception):
            raise value
        if id is not None:
            assert value["id"] == id, value
        return value

    try:
        ranking_file = root / "rank.go"
        ranking_text = "package sample\ntype Engine struct{}\nfunc (e *Engine) EngineTransaction() {}\nfunc EngineTransactionFactory() {}\n"
        ranking_file.write_text(ranking_text, encoding="utf-8")
        send("workspace/symbol", {"query": ""}, 0)
        assert receive(0)["error"]["code"] == -32002

        send("initialize", {"capabilities": {}}, 1)
        assert receive(1)["result"]["capabilities"]["positionEncoding"] == "utf-16"
        send("initialized", {})
        send("workspace/symbol", {"query": "engtxn kind:method path:rank.go"}, "ranked")
        ranked = receive("ranked")["result"]
        assert len(ranked) == 1, ranked
        assert ranked[0]["name"] == "EngineTransaction"
        assert ranked[0]["containerName"] == "*Engine"

        cli = subprocess.run(
            [binary, "find", "engtxn kind:method path:rank.go", str(root)],
            capture_output=True, text=True, check=True
        )
        assert cli.stdout.strip().endswith("*Engine::EngineTransaction"), cli.stdout

        send("workspace/symbol", {"query": "EngineTransaction"}, "exact-rank")
        assert receive("exact-rank")["result"][0]["name"] == "EngineTransaction"
        send("workspace/symbol", {"query": "kind:bad"}, "bad-filter")
        assert receive("bad-filter")["error"]["code"] == -32602

        send("textDocument/didOpen", {"textDocument": {
            "uri": ranking_file.as_uri(), "languageId": "go", "version": 1,
            "text": "package sample\ntype Engine struct{}\nfunc (e *Engine) PendingContext() {}\n"
        }})
        send("workspace/symbol", {"query": "pndctx kind:method path:rank.go"}, "live-ranking")
        live_ranked = receive("live-ranking")["result"]
        assert len(live_ranked) == 1 and live_ranked[0]["containerName"] == "*Engine"

        send("workspace/symbol", {"query": "engtxn kind:method path:rank.go"}, "shadowed")
        shadowed = receive("shadowed")["result"]
        assert all(
            symbol["name"] != "EngineTransaction"
            for symbol in shadowed
        ), shadowed
        send("textDocument/didClose", {"textDocument": {"uri": ranking_file.as_uri()}})
        print("PASS: ranked CLI/LSP search, filters, receiver context, live-buffer ranking")


        send("workspace/symbol", {"query": "café"}, 2)
        found = receive(2)["result"]
        assert len(found) == 1, found
        assert found[0]["location"]["uri"] == source.as_uri(), found
        assert found[0]["location"]["range"] == {
            "start": {"line": 0, "character": 13},
            "end": {"line": 0, "character": 17},
        }, found

        # Unsaved changes replace disk symbols, without changing CLI results.
        uri = source.as_uri()
        draft = "/* 🦀é */ fn draft_only() {}\n"
        send("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "rust", "version": 1, "text": "/* 🦀é */ fn café() {}\n"
        }})
        send("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": 3},
            "contentChanges": [{"text": draft}]
        })
        send("workspace/symbol", {"query": "draft_only"}, "draft")
        found = receive("draft")["result"]
        assert len(found) == 1, found
        assert found[0]["location"]["range"]["start"]["character"] == 13
        send("workspace/symbol", {"query": "café"}, "old-name")
        assert receive("old-name")["result"] == []
        cli = subprocess.run([binary, "find", "café", str(root)], capture_output=True, text=True, check=True)
        assert "café" in cli.stdout
        cli = subprocess.run([binary, "find", "draft_only", str(root)], capture_output=True, text=True, check=True)
        assert not cli.stdout
        send("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{"text": "fn stale() {}"}]
        })
        send("workspace/symbol", {"query": "draft_only"}, "version")
        assert len(receive("version")["result"]) == 1
        source.write_text(draft, encoding="utf-8")
        send("workspace/symbol", {"query": "draft_only"}, "saved-draft")
        assert len(receive("saved-draft")["result"]) == 1
        cli = subprocess.run([binary, "find", "draft_only", str(root)], capture_output=True, text=True, check=True)
        assert "draft_only" in cli.stdout
        send("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": 4},
            "contentChanges": [{"text": ""}]
        })
        send("workspace/symbol", {"query": "draft_only"}, "empty")
        assert receive("empty")["result"] == []
        send("textDocument/didClose", {"textDocument": {"uri": uri}})
        send("workspace/symbol", {"query": "draft_only"}, "closed")
        assert len(receive("closed")["result"]) == 1
        new_uri = (root / "new.go").as_uri()
        send("textDocument/didOpen", {"textDocument": {
            "uri": new_uri, "languageId": "go", "version": 1,
            "text": "package test\nfunc BrandNew() {}\n"
        }})
        send("workspace/symbol", {"query": "BrandNew"}, "new-file")
        assert len(receive("new-file")["result"]) == 1
        assert not (root / "new.go").exists()
        send("textDocument/didClose", {"textDocument": {"uri": new_uri}})
        send("workspace/symbol", {"query": "BrandNew"}, "discard-new")
        assert receive("discard-new")["result"] == []
        print("PASS: unsaved rename, Unicode, CLI isolation, versions, save/close, new Go buffer")
        source.write_text("fn renamed() {}\n", encoding="utf-8")
        send("workspace/symbol", {"query": "renamed"}, 3)
        assert len(receive(3)["result"]) == 1

        source.unlink()
        send("workspace/symbol", {"query": "renamed"}, 4)
        assert receive(4)["result"] == []

        send("workspace/symbol", {"query": 42}, 5)
        assert receive(5)["error"]["code"] == -32602

        report = subprocess.check_output(
            [binary, "index", str(root)], text=True
        )
        database = pathlib.Path(report.strip().rsplit(" at ", 1)[1])
        lock = sqlite3.connect(database)
        lock.execute("BEGIN EXCLUSIVE")
        send("workspace/symbol", {"query": ""}, "changing-buffer")
        send("textDocument/didOpen", {"textDocument": {
            "uri": source.as_uri(), "version": 1, "languageId": "rust", "text": "fn pending() {}"
        }})
        assert receive("changing-buffer", timeout=2)["error"]["code"] == -32801
        send("textDocument/didClose", {"textDocument": {"uri": source.as_uri()}})


        send("workspace/symbol", {"query": ""}, 6)
        send("$/cancelRequest", {"id": 6})
        send("scope/unsupported", {}, 7)
        responses = {
            value["id"]: value
            for value in [receive(timeout=2), receive(timeout=2)]
        }
        assert responses[6]["error"]["code"] == -32800
        assert responses[7]["error"]["code"] == -32601

        send("workspace/symbol", {"query": ""}, 8)
        send("shutdown", id=9)
        responses = {
            value["id"]: value
            for value in [receive(timeout=2), receive(timeout=2)]
        }
        assert responses[8]["error"]["code"] == -32800
        assert responses[9]["result"] is None

        send("exit")
        # Keep stdin AND the SQLite lock open until the server has exited.
        assert process.wait(timeout=2) == 0
        print(
            "PASS: Unicode, URI escaping, saved edits/deletes, "
            "cancellation, locked-DB shutdown"
        )
    finally:
        if process.poll() is None:
            process.kill()
        process.wait()
        if lock:
            lock.close()
        if database:
            shutil.rmtree(database.parent)
