"""MCP server for operating the oracle host (redroid-1) through the LiteLLM gateway.

oracle is an Oracle Cloud ARM VM outside the k3s cluster. It runs the redroid
container with WeChat and the WAuxiliary FavInbox plugin, the favinbox host
service (fav's WeChat source), ws-scrcpy (redroid.s117.me) and frpc. This
server gives Claude the same kind of access to it that the k8s / k8s_ops MCP
servers give to the cluster, in place of the hermes-oracle A2A agent.

One process, one port, two MCP endpoints with their own bearer token:

  /mcp/read   LiteLLM server `oracle`: read-only tools (fixed commands, path
              allowlists, no shell)
  /mcp/ops    LiteLLM server `oracle_ops`: the read-only tools plus service
              control, WeChat UI actions, deploys from GitHub, file writes and
              a root shell
  /healthz    liveness, no auth

The process runs as root (docker, adb, root-owned files), so the read-only
tier is enforced by this code, not by the operating system. It binds
127.0.0.1; frpc tunnels the port to frps on nas-us, and LiteLLM calls
http://oracle-mcp.proxy.svc.cluster.local/mcp/<tier> with the tier's token.
Any pod in the cluster can reach frps ports, so the token check here is what
keeps the tiers apart, not a NetworkPolicy.

Every tool's output passes through a redactor that masks the secrets this host
holds (both tokens here, favinbox's token, the plugin bridge token, frpc's
auth token) and common token patterns. Every call is logged to the journal
(`journalctl -u oracle-mcp`) with its tier, tool, arguments and outcome.

Env (normally /etc/oracle-mcp.env, root 0600):
    ORACLE_MCP_TOKEN_READ  bearer token for /mcp/read (required, >= 32 chars)
    ORACLE_MCP_TOKEN_OPS   bearer token for /mcp/ops (required, >= 32 chars)
    ORACLE_MCP_HOST        bind address (default 127.0.0.1)
    ORACLE_MCP_PORT        bind port (default 9972)
    ORACLE_MCP_ALLOWED_HOSTS  comma-separated Host header values the MCP
                           endpoints accept (default: the cluster Service name
                           and localhost)
"""

from __future__ import annotations

import asyncio
import contextlib
import functools
import hashlib
import hmac
import io
import json
import logging
import os
import re
import shlex
import shutil
import signal
import stat
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from mcp.server.fastmcp import FastMCP, Image
from mcp.server.fastmcp.exceptions import ToolError
from mcp.server.transport_security import TransportSecuritySettings
from mcp.types import ToolAnnotations
from starlette.applications import Starlette
from starlette.datastructures import Headers
from starlette.responses import JSONResponse
from starlette.routing import Route

log = logging.getLogger("oracle-mcp")

READ_PATH = "/mcp/read"
OPS_PATH = "/mcp/ops"
MIN_TOKEN_LENGTH = 32

ADB_SERIAL = os.environ.get("ORACLE_MCP_ADB_SERIAL", "127.0.0.1:5555")
REDROID_CONTAINER = "redroid"
REDROID_DATA = Path("/home/ubuntu/redroid-data")
WECHAT_DATA = REDROID_DATA / "data/com.tencent.mm"
WECHAT_FILES = REDROID_DATA / "media/0/Android/data/com.tencent.mm/files"
FAVINBOX_DIR = WECHAT_FILES / "FavInbox"
BRIDGE_TOKEN_FILE = WECHAT_FILES / "FavBridge/token"
WAUX_MEDIA = REDROID_DATA / "media/0/Android/media/com.tencent.mm"
PLUGIN_DIR = WAUX_MEDIA / "WAuxiliary/Plugin/FavInbox"
# The same directory as WeChat sees it. Plugin files are written through the
# /sdcard FUSE view; writing the host path directly leaves WeChat reading stale
# content.
PLUGIN_DIR_SDCARD = "/sdcard/Android/media/com.tencent.mm/WAuxiliary/Plugin/FavInbox"
PLUGIN_DIR_DATA = "/data/media/0/Android/media/com.tencent.mm/WAuxiliary/Plugin/FavInbox"
PLUGIN_OWNER = "u0_a68:media_rw"

FAVINBOX_URL = "http://127.0.0.1:9970"
FAVINBOX_ENV = Path("/etc/favinbox.env")
FAVINBOX_SERVER = Path("/opt/favinbox/favinbox_server.py")
FRPC_CONFIG = Path("/etc/frp/frpc.toml")
OWN_ENV = Path("/etc/oracle-mcp.env")
BACKUP_DIR = Path("/var/backups/oracle-mcp")

GITHUB_RAW = "https://raw.githubusercontent.com/cyrahs/fav/{commit}/script/wechat_favinbox/{name}"

# Gateway limits: LiteLLM stops `oracle` calls after 120 s and `oracle_ops`
# calls after 600 s (apps/litellm/configmap.yaml). Stay under them.
READ_TIMEOUT_MAX = 110
OPS_TIMEOUT_MAX = 590

TEXT_LIMIT = 60_000

JOURNAL_UNITS = ("favinbox", "ws-scrcpy", "frpc", "oracle-mcp", "docker", "containerd", "ssh")
STATUS_UNITS = ("favinbox", "ws-scrcpy", "frpc", "oracle-mcp", "docker")
# Units service_action may touch. Stopping frpc or this server would cut every
# later call, so those two may only be restarted, and only with a delay.
CONTROL_UNITS = ("favinbox", "ws-scrcpy", "frpc", "oracle-mcp")
SELF_CUTTING_UNITS = ("frpc", "oracle-mcp")

READ_ROOTS = (
    Path("/opt/favinbox"),
    Path("/opt/ws-scrcpy"),
    Path("/opt/oracle-mcp"),
    # Only these units: another unit's Environment= lines may hold a secret
    # that matches no redaction pattern.
    *(Path(f"/etc/systemd/system/{u}.service") for u in ("favinbox", "ws-scrcpy", "frpc", "oracle-mcp")),
    Path("/etc/frp"),
    BACKUP_DIR,
    FAVINBOX_DIR,
    PLUGIN_DIR,
)
# Directory listings may also show names (never contents) under WeChat's data
# directory, for checking its XWeb and 视频号 caches.
LIST_ROOTS = (*READ_ROOTS, WECHAT_DATA, WAUX_MEDIA)

FAVINBOX_GET = frozenset(
    {
        "/health",
        "/items",
        "/wx/status",
        "/wx/events",
        "/wx/history",
        "/wx/message",
        "/wx/ui/dump",
        "/wx/ui/selftest",
        "/wx/friends",
        "/wx/officials",
        "/wx/groups",
        "/wx/group/members",
        "/wx/contact",
        "/wx/labels",
        "/wx/label/contacts",
    }
)
# Only routes that open, close or acknowledge things. The plugin's routes that
# act on WeChat's behalf towards other people (send, Moments, revoke, friend
# requests) are deliberately missing.
FAVINBOX_POST = frozenset({"/link/open", "/post/page", "/finder/video", "/wx/ui/open", "/wx/ui/close"})
FAVINBOX_ACK = re.compile(r"^/ack/\d+$")

COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
SINCE_RELATIVE = re.compile(r"^(\d+)(s|min|h|d|w)$")
SINCE_ABSOLUTE = re.compile(r"^\d{4}-\d{2}-\d{2}( \d{2}:\d{2}(:\d{2})?)?$")
KEYCODE_RE = re.compile(r"^(KEYCODE_[A-Z0-9_]+|\d{1,3})$")


# ---------- redaction ----------

_MIN_SECRET_LENGTH = 8
MASK = "***"
_PATTERNS = (
    re.compile(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{8,}"),
    re.compile(r"""(?i)((?:auth\.)?token["']?\s*[=:]\s*["']?)[^\s"',}]{8,}"""),
    re.compile(r"""(?i)(aes_?key["']?\s*[=:]\s*["']?)[0-9A-Za-z+/=]{8,}"""),
)


def _env_values(path: Path) -> list[str]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return []
    values = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        values.append(line.split("=", 1)[1].strip().strip("'\""))
    return values


def _frpc_secrets() -> list[str]:
    try:
        config = tomllib.loads(FRPC_CONFIG.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return []
    token = config.get("auth", {}).get("token")
    return [token] if isinstance(token, str) else []


def collect_secrets() -> list[str]:
    """Every secret this host holds that a tool's output could contain."""
    secrets = [
        os.environ.get("ORACLE_MCP_TOKEN_READ", ""),
        os.environ.get("ORACLE_MCP_TOKEN_OPS", ""),
        *_env_values(OWN_ENV),
        *_env_values(FAVINBOX_ENV),
        *_frpc_secrets(),
    ]
    with contextlib.suppress(OSError):
        secrets.append(BRIDGE_TOKEN_FILE.read_text(encoding="utf-8").strip())
    return secrets


class Redactor:
    def __init__(self, secrets: list[str]) -> None:
        # Longest first, so a secret that contains another is masked whole.
        self._secrets = sorted({s for s in secrets if len(s) >= _MIN_SECRET_LENGTH}, key=len, reverse=True)

    def __call__(self, text: str) -> str:
        for secret in self._secrets:
            if secret in text:
                text = text.replace(secret, MASK)
        for pattern in _PATTERNS:
            text = pattern.sub(lambda m: m.group(1) + MASK, text)
        return text

    def value(self, obj: Any) -> Any:
        """Redact every string inside a JSON-like value."""
        if isinstance(obj, str):
            return self(obj)
        if isinstance(obj, dict):
            return {k: self.value(v) for k, v in obj.items()}
        if isinstance(obj, list | tuple):
            return [self.value(v) for v in obj]
        return obj


def redactor() -> Redactor:
    # Re-read on every call: favinbox's or the bridge's token may have been
    # rotated or the plugin reinstalled since the last one.
    return Redactor(collect_secrets())


def clip(text: str, limit: int = TEXT_LIMIT, *, keep: Literal["head", "tail"] = "head") -> str:
    if len(text) <= limit:
        return text
    cut = len(text) - limit
    if keep == "tail":
        return f"[{cut} earlier characters cut]\n" + text[-limit:]
    return text[:limit] + f"\n[{cut} more characters cut]"


# ---------- processes ----------


@dataclass
class Result:
    rc: int | None
    stdout: bytes
    stderr: bytes
    timed_out: bool
    seconds: float

    def text(self, *, limit: int = TEXT_LIMIT, keep: Literal["head", "tail"] = "head") -> dict[str, Any]:
        red = redactor()
        return {
            "exit_code": self.rc,
            "timed_out": self.timed_out,
            "seconds": round(self.seconds, 2),
            "stdout": clip(red(self.stdout.decode("utf-8", "replace")), limit, keep=keep),
            "stderr": clip(red(self.stderr.decode("utf-8", "replace")), limit // 4, keep=keep),
        }


async def run(argv: list[str], *, timeout: float = 30, stdin: bytes | None = None) -> Result:
    """Run a command without a shell; kill its whole process group on timeout."""
    started = time.monotonic()
    proc = await asyncio.create_subprocess_exec(
        *argv,
        stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        start_new_session=True,
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(stdin), timeout)
        timed_out = False
    except TimeoutError:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
        out, err = await proc.communicate()
        timed_out = True
    return Result(proc.returncode, out, err, timed_out, time.monotonic() - started)


def adb(*args: str) -> list[str]:
    return ["adb", "-s", ADB_SERIAL, *args]


def adb_shell(command: str) -> list[str]:
    """adb shell joins its arguments into one device shell command line."""
    return adb("shell", command)


def adb_root(command: str) -> list[str]:
    # MagiskSU in redroid takes the uid first.
    return adb("shell", f"su 0 -c {shlex.quote(command)}")


async def text_of(argv: list[str], timeout: float = 20) -> str:
    res = await run(argv, timeout=timeout)
    out = res.stdout.decode("utf-8", "replace").strip()
    if res.rc != 0 or res.timed_out:
        err = res.stderr.decode("utf-8", "replace").strip()
        return f"[exit {res.rc}{', timed out' if res.timed_out else ''}] {out} {err}".strip()
    return out


def bounded(timeout: float, limit: int) -> float:
    if timeout <= 0:
        raise ToolError("timeout must be positive")
    return min(timeout, limit)


# ---------- paths ----------


def _denied(path: Path) -> bool:
    return path.name.endswith(".env") or path == BRIDGE_TOKEN_FILE or path.name == "token"


def allowed_path(raw: str, roots: tuple[Path, ...]) -> Path:
    """Resolve symlinks and `..`, then require the path to sit under one of roots."""
    if not raw.startswith("/"):
        raise ToolError("path must be absolute")
    path = Path(os.path.realpath(raw))
    if _denied(path):
        raise ToolError(f"{path} holds a secret and is not readable through this server")
    for root in roots:
        if path == root or root in path.parents:
            return path
    raise ToolError(f"{path} is outside the readable directories: {', '.join(str(r) for r in roots)}")


def backup(path: Path) -> str | None:
    """Copy an existing file under BACKUP_DIR/<timestamp>/<its path>; return the copy's path."""
    if not path.is_file():
        return None
    dest = BACKUP_DIR / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) / path.relative_to("/")
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(path, dest)
    return str(dest)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


# ---------- favinbox ----------


def favinbox_token() -> str:
    for line in FAVINBOX_ENV.read_text(encoding="utf-8").splitlines():
        if line.strip().startswith("FAVINBOX_TOKEN="):
            return line.split("=", 1)[1].strip().strip("'\"")
    raise ToolError(f"FAVINBOX_TOKEN not found in {FAVINBOX_ENV}")


def _favinbox_request(method: str, path: str, query: dict[str, Any] | None, body: Any, timeout: float) -> dict[str, Any]:
    url = FAVINBOX_URL + path
    if query:
        url += "?" + urllib.parse.urlencode({k: str(v) for k, v in query.items()})
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {favinbox_token()}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            status, ctype, payload = resp.status, resp.headers.get("Content-Type", ""), resp.read()
    except urllib.error.HTTPError as exc:
        status, ctype, payload = exc.code, exc.headers.get("Content-Type", ""), exc.read()
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise ToolError(f"favinbox unreachable: {exc}") from None
    result: dict[str, Any] = {"status": status, "content_type": ctype, "bytes": len(payload)}
    if "json" in ctype or "text" in ctype:
        text = payload.decode("utf-8", "replace")
        try:
            result["json"] = json.loads(text)
        except ValueError:
            result["text"] = clip(text)
    # Binary answers (an MP4 from /finder/video, an image) are summarised only.
    return result


async def favinbox_request(method: str, path: str, query: dict[str, Any] | None, body: Any, timeout: float) -> dict[str, Any]:
    result = await asyncio.to_thread(_favinbox_request, method, path, query, body, timeout)
    result = redactor().value(result)
    if "json" in result:
        dumped = json.dumps(result["json"], ensure_ascii=False)
        if len(dumped) > TEXT_LIMIT:
            result["text"] = clip(dumped)
            del result["json"]
    return result


# ---------- read-only tools ----------


async def host_status() -> dict[str, Any]:
    """Uptime and load, memory, root disk usage, the state of the host's services (favinbox,
    ws-scrcpy, frpc, oracle-mcp, docker) and the Docker containers."""
    units = {}
    for unit in STATUS_UNITS:
        units[unit] = await text_of(["systemctl", "show", unit, "--property=ActiveState,SubState,ActiveEnterTimestamp,NRestarts", "--value"])
    return {
        "uptime": await text_of(["uptime"]),
        "memory": await text_of(["free", "-h"]),
        "disk": await text_of(["df", "-h", "/"]),
        "units": {u: " | ".join(v.splitlines()) for u, v in units.items()},
        "containers": await text_of(["docker", "ps", "-a", "--format", "{{.Names}}  {{.Image}}  {{.Status}}"]),
    }


async def service_logs(unit: str, since: str = "1h", lines: int = 200, grep: str | None = None) -> dict[str, Any]:
    """Journal lines of one host unit, newest last.

    unit: favinbox, ws-scrcpy, frpc, oracle-mcp, docker, containerd, ssh, or "kernel" for kernel
    messages (OOM kills). favinbox logs every /ui open and close with its steps and timings.
    since: relative ("30min", "2h", "1d") or absolute ("2026-10-22" or "2026-10-22 18:00").
    lines: at most 2000. grep: a regular expression the lines must match (journalctl --grep).
    """
    if unit != "kernel" and unit not in JOURNAL_UNITS:
        raise ToolError(f"unit must be one of {', '.join(JOURNAL_UNITS)} or kernel")
    if m := SINCE_RELATIVE.match(since):
        since_arg = f"-{m.group(1)}{m.group(2)}"
    elif SINCE_ABSOLUTE.match(since):
        since_arg = since
    else:
        raise ToolError('since must look like "30min", "2h", "1d" or "2026-10-22 18:00"')
    lines = max(1, min(lines, 2000))
    argv = ["journalctl", "--no-pager", "-o", "short-iso", "--since", since_arg, "-n", str(lines)]
    argv += ["-k"] if unit == "kernel" else ["-u", unit]
    if grep:
        argv += ["--grep", grep]
    res = await run(argv, timeout=60)
    return res.text(keep="tail")


async def frpc_status() -> dict[str, Any]:
    """frpc's tunnels (name, type, local and remote port; never the token), whether it is
    running, and its last 40 journal lines."""
    try:
        config = tomllib.loads(FRPC_CONFIG.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ToolError(f"cannot read {FRPC_CONFIG}: {exc}") from None
    proxies = [
        {k: p.get(k) for k in ("name", "type", "localIP", "localPort", "remotePort")} for p in config.get("proxies", [])
    ]
    return {
        "server": f"{config.get('serverAddr')}:{config.get('serverPort')}",
        "proxies": proxies,
        "active": await text_of(["systemctl", "is-active", "frpc"]),
        "journal": redactor()(await text_of(["journalctl", "--no-pager", "-o", "short-iso", "-u", "frpc", "-n", "40"])),
    }


async def redroid_status() -> dict[str, Any]:
    """The redroid container (state, start time, bridge IP), adb's view of the device, the
    focused window, and WeChat's processes."""
    return {
        "container": await text_of(
            ["docker", "inspect", "-f", "{{.State.Status}} since {{.State.StartedAt}} ip {{.NetworkSettings.IPAddress}}", REDROID_CONTAINER]
        ),
        "adb_devices": await text_of(["adb", "devices"]),
        "boot_completed": await text_of(adb_shell("getprop sys.boot_completed")),
        "focus": await text_of(adb_shell("dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'")),
        "wechat_processes": await text_of(adb_shell("ps -A -o PID,NAME | grep com.tencent.mm")),
    }


async def logcat(pattern: str = "FavBridge", lines: int = 300) -> dict[str, Any]:
    """The last lines of the device's logcat buffer that match a regular expression.

    The FavInbox plugin logs under the tag FavBridge (the default pattern). lines: at most 3000.
    """
    try:
        regex = re.compile(pattern)
    except re.error as exc:
        raise ToolError(f"bad pattern: {exc}") from None
    lines = max(1, min(lines, 3000))
    res = await run(adb("logcat", "-d", "-v", "threadtime"), timeout=60)
    if res.rc != 0:
        return res.text()
    matched = [ln for ln in res.stdout.decode("utf-8", "replace").splitlines() if regex.search(ln)]
    return {"matched": len(matched), "lines": clip(redactor()("\n".join(matched[-lines:])), keep="tail")}


async def screenshot(width: int = 540) -> Image:
    """The redroid screen (1080x1920) as a JPEG, scaled to `width` pixels wide (180 to 1080)."""
    width = max(180, min(width, 1080))
    res = await run(adb("exec-out", "screencap", "-p"), timeout=30)
    if res.rc != 0 or not res.stdout.startswith(b"\x89PNG"):
        raise ToolError(f"screencap failed: {res.stderr.decode('utf-8', 'replace')[:500]}")
    from PIL import Image as PILImage

    img = PILImage.open(io.BytesIO(res.stdout)).convert("RGB")
    if img.width > width:
        img = img.resize((width, round(img.height * width / img.width)), PILImage.Resampling.LANCZOS)
    buf = io.BytesIO()
    img.save(buf, format="JPEG", quality=80)
    return Image(data=buf.getvalue(), format="jpeg")


async def favinbox_get(path: str, query: dict[str, str | int] | None = None) -> dict[str, Any]:
    """GET from the favinbox host service, which adds its own token; /wx/* goes to the plugin's bridge.

    Allowed paths: /health, /items, /wx/status (logged-in account; 502 when WeChat is logged out),
    /wx/events, /wx/history, /wx/message, /wx/ui/dump (what the plugin sees on screen),
    /wx/ui/selftest, /wx/friends, /wx/officials, /wx/groups, /wx/group/members, /wx/contact,
    /wx/labels, /wx/label/contacts. query: the query string as a mapping. Image aes keys are masked.
    """
    if path not in FAVINBOX_GET:
        raise ToolError(f"path must be one of {', '.join(sorted(FAVINBOX_GET))}")
    return await favinbox_request("GET", path, query, None, 60)


async def read_file(path: str, offset: int = 0, max_bytes: int = 100_000) -> dict[str, Any]:
    """Read a text file under /opt/favinbox, /opt/ws-scrcpy, /opt/oracle-mcp, /etc/frp,
    /var/backups/oracle-mcp, the FavInbox inbox or the installed FavInbox plugin, or the unit file
    of favinbox, ws-scrcpy, frpc or oracle-mcp.

    Secret files (*.env, the bridge token) are refused and known secrets are masked.
    offset/max_bytes page through large files (max_bytes at most 200000).
    """
    p = allowed_path(path, READ_ROOTS)
    if not p.is_file():
        raise ToolError(f"{p} is not a file")
    max_bytes = max(1, min(max_bytes, 200_000))
    size = p.stat().st_size
    with p.open("rb") as fh:
        fh.seek(max(0, offset))
        data = fh.read(max_bytes)
    return {
        "path": str(p),
        "size": size,
        "offset": offset,
        "content": redactor()(data.decode("utf-8", "replace")),
        "more": offset + len(data) < size,
    }


async def list_dir(path: str) -> dict[str, Any]:
    """List a directory (names, types, sizes, modification times; up to 500 entries, newest first).

    Allowed under the read_file directories plus WeChat's data directory
    (/home/ubuntu/redroid-data/data/com.tencent.mm, e.g. cache/xweb_cache or the 视频号 cache)
    and WAuxiliary's media directory. Only names are shown there, never file contents.
    """
    p = allowed_path(path, LIST_ROOTS)
    if not p.is_dir():
        raise ToolError(f"{p} is not a directory")
    entries = []
    with os.scandir(p) as it:
        for entry in it:
            try:
                st = entry.stat(follow_symlinks=False)
            except OSError:
                continue
            kind = "dir" if stat.S_ISDIR(st.st_mode) else "link" if stat.S_ISLNK(st.st_mode) else "file"
            entries.append({"name": entry.name, "type": kind, "size": st.st_size, "mtime": int(st.st_mtime)})
    entries.sort(key=lambda e: e["mtime"], reverse=True)
    return {"path": str(p), "count": len(entries), "entries": entries[:500]}


# ---------- ops tools ----------


async def service_action(unit: str, action: Literal["start", "stop", "restart"]) -> dict[str, Any]:
    """Start, stop or restart a host unit (favinbox, ws-scrcpy, frpc, oracle-mcp) or the redroid container.

    frpc and oracle-mcp carry this very call, so they can only be restarted, and the restart runs
    three seconds later from a transient systemd timer: the call returns first, then the tunnel
    drops for a few seconds. Restarting redroid restarts Android and WeChat (the plugin reloads).
    """
    if unit == REDROID_CONTAINER:
        res = await run(["docker", action, REDROID_CONTAINER], timeout=180)
        return res.text()
    if unit not in CONTROL_UNITS:
        raise ToolError(f"unit must be one of {', '.join(CONTROL_UNITS)} or {REDROID_CONTAINER}")
    if unit in SELF_CUTTING_UNITS:
        if action != "restart":
            raise ToolError(f"{unit} carries this connection; it can only be restarted")
        res = await run(["systemd-run", "--on-active=3", "--timer-property=AccuracySec=100ms", "systemctl", "restart", unit])
        return {**res.text(), "note": f"{unit} restarts in about 3 s; the next call may fail for a few seconds"}
    res = await run(["systemctl", action, unit], timeout=120)
    return {**res.text(), "state": await text_of(["systemctl", "is-active", unit])}


async def favinbox_post(path: str, body: dict[str, Any] | None = None, timeout: int = 180) -> dict[str, Any]:
    """POST to the favinbox host service (it adds its own token).

    Allowed paths: /wx/ui/open and /wx/ui/close (the plugin taps a message open or goes back,
    body {"talker", "msg_id", "texts"}), /link/open, /post/page, /finder/video (open a message
    in WeChat and wait for what it loaded; body {"url" or "object_id", "message": {...}}), and
    /ack/<msg_id>. Sending messages, Moments posts, revokes and friend requests are not exposed.
    Binary answers (an MP4, an image) are summarised by size. timeout: seconds, at most 590.
    """
    if path not in FAVINBOX_POST and not FAVINBOX_ACK.match(path):
        raise ToolError(f"path must be one of {', '.join(sorted(FAVINBOX_POST))} or /ack/<msg_id>")
    return await favinbox_request("POST", path, None, body if body is not None else {}, bounded(timeout, OPS_TIMEOUT_MAX))


async def android_input(
    action: Literal["tap", "swipe", "key", "text"],
    x: int | None = None,
    y: int | None = None,
    x2: int | None = None,
    y2: int | None = None,
    duration_ms: int = 300,
    key: str | None = None,
    text: str | None = None,
) -> dict[str, Any]:
    """Send one input event to redroid through adb, the way the ws-scrcpy console does by hand.

    For one-off fixes such as a confirmation dialog, not for crawling: fav opens messages with
    the plugin's simulated taps (favinbox_post /wx/ui/open), never with adb input.
    tap: x, y. swipe: x, y to x2, y2 over duration_ms. key: KEYCODE_BACK, KEYCODE_HOME, ... or a
    number. text: printable ASCII typed into the focused field. Screen is 1080x1920.
    """

    def coord(v: int | None, name: str) -> int:
        if v is None or not 0 <= v <= 4000:
            raise ToolError(f"{name} must be an integer from 0 to 4000")
        return v

    if action == "tap":
        cmd = f"input tap {coord(x, 'x')} {coord(y, 'y')}"
    elif action == "swipe":
        dur = max(1, min(duration_ms, 10_000))
        cmd = f"input swipe {coord(x, 'x')} {coord(y, 'y')} {coord(x2, 'x2')} {coord(y2, 'y2')} {dur}"
    elif action == "key":
        if not key or not KEYCODE_RE.match(key):
            raise ToolError("key must look like KEYCODE_BACK or a key number")
        cmd = f"input keyevent {key}"
    else:
        if not text or not all(32 <= ord(c) < 127 for c in text) or len(text) > 500:
            raise ToolError("text must be 1 to 500 printable ASCII characters")
        # `input text` takes %s for a space.
        cmd = "input text " + shlex.quote(text.replace(" ", "%s"))
    res = await run(adb_shell(cmd), timeout=30)
    return res.text()


async def wechat_restart() -> dict[str, Any]:
    """Force-stop WeChat and start it again at its main screen (com.tencent.mm/.ui.LauncherUI).

    This is how a newly installed FavInbox plugin takes effect. The bridge answers again once
    WeChat and WAuxiliary have loaded, usually within 30 s; check with favinbox_get /wx/status.
    """
    stop = await run(adb_shell("am force-stop com.tencent.mm"), timeout=30)
    await asyncio.sleep(2)
    start = await run(adb_shell("am start -n com.tencent.mm/.ui.LauncherUI"), timeout=30)
    return {"force_stop": stop.text(), "start": start.text()}


def _fetch(commit: str, name: str) -> bytes:
    url = GITHUB_RAW.format(commit=commit, name=name)
    try:
        with urllib.request.urlopen(url, timeout=30) as resp:
            return resp.read()
    except (urllib.error.URLError, TimeoutError, OSError) as exc:
        raise ToolError(f"fetching {url} failed: {exc}") from None


async def deploy_from_github(
    component: Literal["favinbox_server", "favinbox_plugin"], commit: str, restart: bool = True
) -> dict[str, Any]:
    """Install a FavInbox file from cyrahs/fav (script/wechat_favinbox/) at one commit.

    commit: a full 40-character SHA. The current files are backed up under /var/backups/oracle-mcp
    first and the sha256 of old and new files is reported.
    favinbox_server: favinbox_server.py to /opt/favinbox, byte-compiled first; restart=True then
    restarts favinbox and waits for /health.
    favinbox_plugin: info.prop and main.java into WAuxiliary's plugin folder through /sdcard (so
    WeChat sees the new content), owned u0_a68:media_rw mode 660. WeChat keeps the old plugin
    until it restarts (wechat_restart) or the plugin is reloaded in WAuxiliary; restart is ignored.
    """
    if not COMMIT_RE.match(commit):
        raise ToolError("commit must be a full 40-character lowercase SHA")
    if component == "favinbox_server":
        new = await asyncio.to_thread(_fetch, commit, "favinbox_server.py")
        old = FAVINBOX_SERVER.read_bytes() if FAVINBOX_SERVER.is_file() else b""
        with tempfile.TemporaryDirectory() as tmp:
            candidate = Path(tmp) / "favinbox_server.py"
            candidate.write_bytes(new)
            check = await run(["/usr/bin/python3", "-m", "py_compile", str(candidate)], timeout=60)
            if check.rc != 0:
                raise ToolError(f"favinbox_server.py at {commit} does not compile: {check.text()['stderr']}")
        saved = backup(FAVINBOX_SERVER)
        tmp_path = FAVINBOX_SERVER.with_suffix(".py.new")
        tmp_path.write_bytes(new)
        tmp_path.chmod(0o644)
        os.replace(tmp_path, FAVINBOX_SERVER)
        result: dict[str, Any] = {"old_sha256": sha256(old), "new_sha256": sha256(new), "backup": saved}
        if restart:
            await run(["systemctl", "restart", "favinbox"], timeout=60)
            health = {}
            for _ in range(10):
                await asyncio.sleep(1)
                health = await favinbox_request("GET", "/health", None, None, 5)
                if health.get("status") == 200:
                    break
            result["state"] = await text_of(["systemctl", "is-active", "favinbox"])
            result["health"] = health
        return result

    names = ("info.prop", "main.java")
    files = {name: await asyncio.to_thread(_fetch, commit, name) for name in names}
    saved = [backup(PLUGIN_DIR / name) for name in names]
    stage = "/data/local/tmp/oracle-mcp-plugin"
    script = "\n".join(
        [
            "set -e",
            *[f"cat {stage}/{n} > {PLUGIN_DIR_SDCARD}/{n}" for n in names],
            "sync",
            f"chown {PLUGIN_OWNER} {PLUGIN_DIR_DATA}/*",
            f"chmod 660 {PLUGIN_DIR_DATA}/*",
            f"rm -rf {stage}",
        ]
    )
    with tempfile.TemporaryDirectory() as tmp:
        for name, data in files.items():
            (Path(tmp) / name).write_bytes(data)
        (Path(tmp) / "install.sh").write_text(script + "\n", encoding="utf-8")
        steps = [await run(adb_shell(f"mkdir -p {stage}"), timeout=30)]
        steps.append(await run(adb("push", *(str(Path(tmp) / n) for n in (*names, "install.sh")), stage + "/"), timeout=60))
        steps.append(await run(adb_root(f"sh {stage}/install.sh"), timeout=60))
    for step in steps:
        if step.rc != 0:
            raise ToolError(f"plugin install failed: {step.text()}")
    installed = await text_of(adb_root(f"sha256sum {PLUGIN_DIR_SDCARD}/info.prop {PLUGIN_DIR_SDCARD}/main.java"))
    version = await text_of(adb_root(f"cat {PLUGIN_DIR_SDCARD}/info.prop"))
    return {
        "expected_sha256": {n: sha256(d) for n, d in files.items()},
        "installed_sha256": installed,
        "info_prop": version,
        "backup": saved,
        "note": "WeChat still runs the old plugin until wechat_restart or a reload in WAuxiliary",
    }


async def write_file(path: str, content: str, mode: str | None = None, make_dirs: bool = False) -> dict[str, Any]:
    """Write a UTF-8 text file on the host (as root), replacing it atomically.

    An existing file is backed up under /var/backups/oracle-mcp first and keeps its owner and
    mode unless mode (octal, e.g. "644") is given. make_dirs creates missing parent directories.
    Files inside redroid that WeChat reads must go through /sdcard (see deploy_from_github),
    not their host path.
    """
    if not path.startswith("/"):
        raise ToolError("path must be absolute")
    p = Path(os.path.realpath(path))
    if mode is not None and not re.fullmatch(r"[0-7]{3,4}", mode):
        raise ToolError('mode must be octal like "644"')
    if not p.parent.is_dir():
        if not make_dirs:
            raise ToolError(f"{p.parent} does not exist (pass make_dirs=true)")
        p.parent.mkdir(parents=True)
    data = content.encode("utf-8")
    old = p.stat() if p.exists() else None
    saved = backup(p)
    fd, tmp_name = tempfile.mkstemp(dir=p.parent, prefix=f".{p.name}.")
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
        if mode is not None:
            os.chmod(tmp_name, int(mode, 8))
        elif old is not None:
            os.chmod(tmp_name, stat.S_IMODE(old.st_mode))
        else:
            os.chmod(tmp_name, 0o644)
        if old is not None:
            os.chown(tmp_name, old.st_uid, old.st_gid)
        os.replace(tmp_name, p)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(tmp_name)
        raise
    return {"path": str(p), "bytes": len(data), "sha256": sha256(data), "backup": saved}


async def shell(command: str, timeout: int = 120) -> dict[str, Any]:
    """Run a command with `bash -lc` as root on the oracle host (niced, no stdin).

    Inside Android: `adb -s 127.0.0.1:5555 shell su 0 -c '<cmd>'` or
    `docker exec redroid su 0 -c '<cmd>'`. timeout: seconds, at most 590; for longer jobs use
    `nohup <cmd> > /tmp/<job>.log 2>&1 &` and read the log in a later call. Output is cut to the
    last 60000 characters and known secrets are masked.
    """
    res = await run(["nice", "-n", "10", "bash", "-lc", command], timeout=bounded(timeout, OPS_TIMEOUT_MAX))
    return res.text(keep="tail")


READ_TOOLS = (host_status, service_logs, frpc_status, redroid_status, logcat, screenshot, favinbox_get, read_file, list_dir)
OPS_TOOLS = (service_action, favinbox_post, android_input, wechat_restart, deploy_from_github, write_file, shell)


# ---------- servers ----------

_COMMON = """\
The oracle host (hostname redroid-1): an Oracle Cloud ARM64 VM (2 vCPU, 11 GiB, Ubuntu 24.04)
outside the k3s cluster. It runs the Docker container `redroid` (Android 12, adb at 127.0.0.1:5555,
/data bind-mounted from /home/ubuntu/redroid-data) with WeChat and the WAuxiliary plugin FavInbox
(source: cyrahs/fav script/wechat_favinbox/); the systemd service favinbox (127.0.0.1:9970, fav's
WeChat source reaches it through frp, /wx/* is forwarded to the plugin's bridge); ws-scrcpy
(127.0.0.1:8000, the redroid.s117.me console); and frpc, which carries all of them and this server
to frps on nas-us.

WeChat rules: the account gets risk-controlled easily. Do not change device fingerprints or hide
root, do not automate WeChat with adb input (fav opens messages through the plugin's simulated
taps), and do not stop or start fav's WeChat source unless the user asks. Never print tokens or
secrets; outputs are masked, keep it that way."""

READ_INSTRUCTIONS = (
    _COMMON
    + """

This is the read-only tier: status, journals, logcat, screenshots, favinbox reads and file reads
under fixed directories. Restarts, deploys, WeChat UI actions and a root shell are on the
oracle_ops server."""
)

OPS_INSTRUCTIONS = (
    _COMMON
    + """

This is the full-access tier: every read-only tool plus service control, WeChat UI actions,
FavInbox deploys from GitHub, file writes and a root shell, so use it only for what the user asked
for. Restarting frpc or oracle-mcp cuts the connection for a few seconds. The gateway stops a call
after 600 s; start longer jobs with nohup and read their log later. Files are backed up under
/var/backups/oracle-mcp before they are replaced."""
)


def _audited(tier: str, fn: Any) -> Any:
    @functools.wraps(fn)
    async def wrapper(*args: Any, **kwargs: Any) -> Any:
        shown = {k: (f"<{len(v)} chars sha256 {sha256(v.encode())[:12]}>" if k == "content" else v) for k, v in kwargs.items()}
        started = time.monotonic()
        outcome = "ok"
        try:
            return await fn(*args, **kwargs)
        except ToolError as exc:
            outcome = f"refused: {exc}"
            raise
        except Exception as exc:
            outcome = f"error: {exc!r}"
            log.exception("tool %s failed", fn.__name__)
            raise ToolError(f"{fn.__name__} failed: {redactor()(repr(exc))}") from None
        finally:
            entry = {"tier": tier, "tool": fn.__name__, "args": shown, "outcome": outcome, "seconds": round(time.monotonic() - started, 2)}
            log.info("call %s", redactor()(json.dumps(entry, ensure_ascii=False, default=str)))

    return wrapper


def build_server(tier: Literal["read", "ops"], allowed_hosts: list[str]) -> FastMCP:
    mcp = FastMCP(
        name="oracle" if tier == "read" else "oracle_ops",
        instructions=READ_INSTRUCTIONS if tier == "read" else OPS_INSTRUCTIONS,
        streamable_http_path=READ_PATH if tier == "read" else OPS_PATH,
        stateless_http=True,
        json_response=True,
        transport_security=TransportSecuritySettings(
            enable_dns_rebinding_protection=True, allowed_hosts=allowed_hosts, allowed_origins=[]
        ),
    )
    for fn in READ_TOOLS:
        mcp.add_tool(_audited(tier, fn), annotations=ToolAnnotations(readOnlyHint=True, openWorldHint=False))
    if tier == "ops":
        for fn in OPS_TOOLS:
            mcp.add_tool(_audited(tier, fn), annotations=ToolAnnotations(readOnlyHint=False, destructiveHint=True, openWorldHint=False))
    return mcp


class BearerAuth:
    """Require each MCP endpoint's own bearer token; /healthz is open."""

    def __init__(self, app: Any, tokens: dict[str, str]) -> None:
        self.app = app
        self.tokens = tokens

    async def __call__(self, scope: Any, receive: Any, send: Any) -> None:
        if scope["type"] == "http" and scope["path"] != "/healthz":
            expected = self.tokens.get(scope["path"].rstrip("/"))
            header = Headers(scope=scope).get("authorization", "")
            given = header[7:] if header[:7].lower() == "bearer " else ""
            if expected is None:
                await JSONResponse({"error": "not found"}, status_code=404)(scope, receive, send)
                return
            if not given or not hmac.compare_digest(given.encode(), expected.encode()):
                log.warning("unauthorized request to %s", scope["path"])
                await JSONResponse({"error": "unauthorized"}, status_code=401)(scope, receive, send)
                return
        await self.app(scope, receive, send)


def load_tokens() -> dict[str, str]:
    read = os.environ.get("ORACLE_MCP_TOKEN_READ", "")
    ops = os.environ.get("ORACLE_MCP_TOKEN_OPS", "")
    if len(read) < MIN_TOKEN_LENGTH or len(ops) < MIN_TOKEN_LENGTH:
        raise SystemExit(f"ORACLE_MCP_TOKEN_READ and ORACLE_MCP_TOKEN_OPS must be set (>= {MIN_TOKEN_LENGTH} chars)")
    if read == ops:
        raise SystemExit("ORACLE_MCP_TOKEN_READ and ORACLE_MCP_TOKEN_OPS must differ")
    return {READ_PATH: read, OPS_PATH: ops}


def build_app(tokens: dict[str, str], allowed_hosts: list[str]) -> Any:
    servers = [build_server("read", allowed_hosts), build_server("ops", allowed_hosts)]
    apps = [s.streamable_http_app() for s in servers]

    @contextlib.asynccontextmanager
    async def lifespan(_app: Starlette):  # noqa: ANN202
        async with contextlib.AsyncExitStack() as stack:
            for s in servers:
                await stack.enter_async_context(s.session_manager.run())
            yield

    async def healthz(_request: Any) -> JSONResponse:
        return JSONResponse({"ok": True})

    routes = [Route("/healthz", healthz)]
    for a in apps:
        routes.extend(a.routes)
    return BearerAuth(Starlette(routes=routes, lifespan=lifespan), tokens)


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(name)s: %(message)s", stream=sys.stderr)
    # The SDK logs every request at INFO; the audit line per call is enough.
    logging.getLogger("mcp").setLevel(logging.WARNING)
    tokens = load_tokens()
    allowed_hosts = [
        h.strip()
        for h in os.environ.get(
            "ORACLE_MCP_ALLOWED_HOSTS",
            "oracle-mcp.proxy.svc.cluster.local,oracle-mcp.proxy.svc,127.0.0.1:*,localhost:*",
        ).split(",")
        if h.strip()
    ]
    import uvicorn

    uvicorn.run(
        build_app(tokens, allowed_hosts),
        host=os.environ.get("ORACLE_MCP_HOST", "127.0.0.1"),
        port=int(os.environ.get("ORACLE_MCP_PORT", "9972")),
        log_level="warning",
        proxy_headers=False,
    )


if __name__ == "__main__":
    main()
