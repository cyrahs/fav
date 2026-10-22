"""Tests for server.py: run with `uv run pytest` in this directory."""

from __future__ import annotations

import asyncio
import io
import json
import logging
from pathlib import Path

import pytest
from mcp.server.fastmcp.exceptions import ToolError
from starlette.testclient import TestClient

import server

READ_TOKEN = "r" * 40
OPS_TOKEN = "o" * 40
HEADERS = {"accept": "application/json, text/event-stream", "content-type": "application/json"}


def rpc(client: TestClient, path: str, token: str | None, method: str, params: dict | None = None):
    headers = dict(HEADERS)
    if token is not None:
        headers["authorization"] = f"Bearer {token}"
    body = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    return client.post(path, headers=headers, content=json.dumps(body))


INIT = {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}


@pytest.fixture
def client():
    app = server.build_app({server.READ_PATH: READ_TOKEN, server.OPS_PATH: OPS_TOKEN}, ["testserver"])
    with TestClient(app) as c:
        yield c


def tool_names(client: TestClient, path: str, token: str) -> set[str]:
    resp = rpc(client, path, token, "tools/list")
    assert resp.status_code == 200, resp.text
    return {t["name"] for t in resp.json()["result"]["tools"]}


def test_healthz_needs_no_token(client):
    assert client.get("/healthz").json() == {"ok": True}


def test_each_endpoint_needs_its_own_token(client):
    assert rpc(client, server.READ_PATH, None, "initialize", INIT).status_code == 401
    assert rpc(client, server.READ_PATH, OPS_TOKEN, "initialize", INIT).status_code == 401
    assert rpc(client, server.OPS_PATH, READ_TOKEN, "initialize", INIT).status_code == 401
    assert rpc(client, "/mcp/other", READ_TOKEN, "initialize", INIT).status_code == 404
    assert rpc(client, server.READ_PATH, READ_TOKEN, "initialize", INIT).status_code == 200
    assert rpc(client, server.OPS_PATH, OPS_TOKEN, "initialize", INIT).status_code == 200


def test_unknown_host_is_rejected():
    app = server.build_app({server.READ_PATH: READ_TOKEN, server.OPS_PATH: OPS_TOKEN}, ["oracle-mcp.proxy.svc.cluster.local"])
    with TestClient(app) as c:
        assert rpc(c, server.READ_PATH, READ_TOKEN, "initialize", INIT).status_code == 421


def test_read_tier_has_no_write_tools(client):
    read = tool_names(client, server.READ_PATH, READ_TOKEN)
    ops = tool_names(client, server.OPS_PATH, OPS_TOKEN)
    assert read == {fn.__name__ for fn in server.READ_TOOLS}
    assert ops == read | {fn.__name__ for fn in server.OPS_TOOLS}
    assert "shell" not in read and "shell" in ops


def test_tool_schemas_keep_parameters(client):
    resp = rpc(client, server.READ_PATH, READ_TOKEN, "tools/list")
    tools = {t["name"]: t for t in resp.json()["result"]["tools"]}
    assert tools["service_logs"]["inputSchema"]["required"] == ["unit"]
    assert set(tools["service_logs"]["inputSchema"]["properties"]) == {"unit", "since", "lines", "grep"}


def test_redactor_masks_known_secrets_and_patterns():
    red = server.Redactor(["s3cretvalue123", "short"])
    text = 'a s3cretvalue123 b short Authorization: Bearer abcdefghijkl token = "zzzzzzzzzz" "aes_key": "00112233445566778899"'
    out = red(text)
    assert "s3cretvalue123" not in out
    assert "short" in out  # too short to be treated as a secret
    assert "abcdefghijkl" not in out and "zzzzzzzzzz" not in out and "00112233445566778899" not in out
    assert red.value({"x": ["s3cretvalue123"]}) == {"x": ["***"]}


def test_allowed_path(tmp_path: Path):
    root = tmp_path / "root"
    root.mkdir()
    (root / "ok.txt").write_text("x")
    (root / "favinbox.env").write_text("x")
    (tmp_path / "outside.txt").write_text("x")
    (root / "escape").symlink_to(tmp_path / "outside.txt")
    assert server.allowed_path(str(root / "ok.txt"), (root,)) == root / "ok.txt"
    for bad in ("relative.txt", str(root / "favinbox.env"), str(tmp_path / "outside.txt"), str(root / "escape"), str(root / ".." / "outside.txt")):
        with pytest.raises(ToolError):
            server.allowed_path(bad, (root,))


@pytest.mark.parametrize("path", ["/wx/send/text", "/file/1", "/wx/../wx/send/text", "/wx/image"])
def test_favinbox_get_allowlist(path):
    with pytest.raises(ToolError):
        asyncio.run(server.favinbox_get(path))


@pytest.mark.parametrize("path", ["/wx/send/text", "/wx/sns/post", "/wx/msg/revoke", "/wx/friend/verify", "/ack/12a"])
def test_favinbox_post_allowlist(path):
    with pytest.raises(ToolError):
        asyncio.run(server.favinbox_post(path, {}))


@pytest.mark.parametrize(
    "kwargs",
    [
        {"action": "tap", "x": 1},
        {"action": "tap", "x": -1, "y": 5},
        {"action": "key", "key": "KEYCODE_BACK; reboot"},
        {"action": "text", "text": "hi\nreboot"},
        {"action": "text", "text": "中文"},
    ],
)
def test_android_input_validation(kwargs):
    with pytest.raises(ToolError):
        asyncio.run(server.android_input(**kwargs))


@pytest.mark.parametrize("since", ["1 hour ago", "-1h; rm", "2026/10/22"])
def test_service_logs_validation(since):
    with pytest.raises(ToolError):
        asyncio.run(server.service_logs("favinbox", since=since))
    with pytest.raises(ToolError):
        asyncio.run(server.service_logs("hermes"))


def test_service_action_keeps_the_tunnel(monkeypatch):
    with pytest.raises(ToolError):
        asyncio.run(server.service_action("frpc", "stop"))
    with pytest.raises(ToolError):
        asyncio.run(server.service_action("oracle-mcp", "stop"))
    with pytest.raises(ToolError):
        asyncio.run(server.service_action("sshd", "restart"))


def test_deploy_needs_full_sha():
    with pytest.raises(ToolError):
        asyncio.run(server.deploy_from_github("favinbox_server", "main"))


def test_write_file_backs_up_and_keeps_mode(tmp_path: Path, monkeypatch):
    monkeypatch.setattr(server, "BACKUP_DIR", tmp_path / "backups")
    target = tmp_path / "conf.txt"
    target.write_text("old")
    target.chmod(0o600)
    out = asyncio.run(server.write_file(str(target), "new"))
    assert target.read_text() == "new"
    assert oct(target.stat().st_mode & 0o777) == "0o600"
    assert Path(out["backup"]).read_text() == "old"
    with pytest.raises(ToolError):
        asyncio.run(server.write_file(str(tmp_path / "missing" / "f.txt"), "x"))


def test_shell_masks_secrets_and_times_out(monkeypatch):
    monkeypatch.setenv("ORACLE_MCP_TOKEN_OPS", OPS_TOKEN)
    out = asyncio.run(server.shell(f"echo {OPS_TOKEN}"))
    assert out["exit_code"] == 0 and OPS_TOKEN not in out["stdout"] and "***" in out["stdout"]
    out = asyncio.run(server.shell("sleep 5", timeout=1))
    assert out["timed_out"] is True


def test_load_tokens_refuses_weak_or_equal(monkeypatch):
    monkeypatch.setenv("ORACLE_MCP_TOKEN_READ", "short")
    monkeypatch.setenv("ORACLE_MCP_TOKEN_OPS", OPS_TOKEN)
    with pytest.raises(SystemExit):
        server.load_tokens()
    monkeypatch.setenv("ORACLE_MCP_TOKEN_READ", OPS_TOKEN)
    with pytest.raises(SystemExit):
        server.load_tokens()


def test_tool_call_through_the_endpoint(client, tmp_path: Path, monkeypatch, caplog):
    caplog.set_level(logging.INFO, logger="oracle-mcp")
    monkeypatch.setattr(server, "LIST_ROOTS", (tmp_path,))
    (tmp_path / "a.txt").write_text("x")
    rpc(client, server.READ_PATH, READ_TOKEN, "initialize", INIT)
    resp = rpc(client, server.READ_PATH, READ_TOKEN, "tools/call", {"name": "list_dir", "arguments": {"path": str(tmp_path)}})
    result = resp.json()["result"]
    assert not result.get("isError"), result
    assert result["structuredContent"]["entries"][0]["name"] == "a.txt"
    resp = rpc(client, server.READ_PATH, READ_TOKEN, "tools/call", {"name": "list_dir", "arguments": {"path": "/root"}})
    assert resp.json()["result"]["isError"] is True
    resp = rpc(client, server.READ_PATH, READ_TOKEN, "tools/call", {"name": "shell", "arguments": {"command": "id"}})
    assert resp.json()["result"]["isError"] is True or "error" in resp.json()
    assert any('"tool": "list_dir"' in r.getMessage() for r in caplog.records)


def test_screenshot_comes_back_as_an_image(client, monkeypatch):
    from PIL import Image as PILImage

    buf = io.BytesIO()
    PILImage.new("RGB", (1080, 1920), "white").save(buf, format="PNG")

    async def fake_run(argv, **_kwargs):
        return server.Result(0, buf.getvalue(), b"", False, 0.1)

    monkeypatch.setattr(server, "run", fake_run)
    resp = rpc(client, server.READ_PATH, READ_TOKEN, "tools/call", {"name": "screenshot", "arguments": {}})
    content = resp.json()["result"]["content"]
    assert content[0]["type"] == "image" and content[0]["mimeType"] == "image/jpeg"
