# ruff: noqa: INP001, S101, PLR2004, SLF001

from __future__ import annotations

import asyncio
import select
import shutil
import socket
import ssl
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import TYPE_CHECKING

import httpx
import pytest

from src.tool import connect_to
from src.tool.connect_to import AsyncConnectToTransport, ConnectToTransport

if TYPE_CHECKING:
    from collections.abc import Iterator
    from pathlib import Path

HOST = 'l2d.su'
SIBLING = 'static.l2d.su'


def _recording_handler(seen: list[httpx.Request]) -> httpx.MockTransport:
    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, text='ok')

    return httpx.MockTransport(handler)


def test_requests_move_to_the_connect_host_while_the_host_does_not_resolve() -> None:
    seen: list[httpx.Request] = []
    lookups: list[str] = []

    def resolves(host: str) -> bool:
        lookups.append(host)
        return False

    transport = ConnectToTransport(host=HOST, connect_host=SIBLING, resolves=resolves)
    transport._inner = _recording_handler(seen)
    with httpx.Client(transport=transport) as client:
        client.get('https://l2d.su/data/ships-CN.json')
        client.get('https://l2d.su/data/ships/CN/10000.json')
        client.get('https://checkip.amazonaws.com/')

    assert [str(request.url) for request in seen] == [
        'https://static.l2d.su/data/ships-CN.json',
        'https://static.l2d.su/data/ships/CN/10000.json',
        'https://checkip.amazonaws.com/',
    ]
    # The origin still has to see its own name, or the CDN cannot route the request.
    assert [request.headers['Host'] for request in seen[:2]] == [HOST, HOST]
    # One lookup per transport: the crawler opens a transport per run.
    assert lookups == [HOST]


def test_requests_are_untouched_once_the_host_resolves_again() -> None:
    seen: list[httpx.Request] = []
    transport = ConnectToTransport(host=HOST, connect_host=SIBLING, resolves=lambda _host: True)
    transport._inner = _recording_handler(seen)
    with httpx.Client(transport=transport) as client:
        client.get('https://l2d.su/data/ships-CN.json')

    assert str(seen[0].url) == 'https://l2d.su/data/ships-CN.json'


def test_the_async_transport_moves_requests_the_same_way() -> None:
    seen: list[httpx.Request] = []
    transport = AsyncConnectToTransport(host=HOST, connect_host=SIBLING, resolves=lambda _host: False)
    transport._inner = _recording_handler(seen)

    async def run() -> None:
        async with httpx.AsyncClient(transport=transport) as client:
            await client.get('https://l2d.su/data/ships-CN.json')

    asyncio.run(run())

    assert str(seen[0].url) == 'https://static.l2d.su/data/ships-CN.json'
    assert seen[0].headers['Host'] == HOST


def test_the_response_keeps_the_url_that_was_asked_for() -> None:
    transport = ConnectToTransport(host=HOST, connect_host=SIBLING, resolves=lambda _host: False)
    transport._inner = _recording_handler([])
    with httpx.Client(transport=transport) as client:
        response = client.get('https://l2d.su/data/ships-CN.json')

    assert str(response.url) == 'https://l2d.su/data/ships-CN.json'


def test_host_resolves_reports_a_missing_record_as_false() -> None:
    assert connect_to.host_resolves('does-not-exist.invalid') is False


# ---- Real TLS: a server holding a certificate only for l2d.su, reached by another address ----


@pytest.fixture(scope='module')
def tls_files(tmp_path_factory: pytest.TempPathFactory) -> dict[str, Path]:
    if shutil.which('openssl') is None:
        pytest.skip('openssl is not installed')
    root = tmp_path_factory.mktemp('tls')
    ca_key, ca_cert = root / 'ca.key', root / 'ca.pem'
    key, csr, cert, ext = root / 'l2d.key', root / 'l2d.csr', root / 'l2d.pem', root / 'l2d.ext'
    ext.write_text(f'subjectAltName=DNS:{HOST}\nbasicConstraints=CA:FALSE\n')

    def openssl(*args: str) -> None:
        subprocess.run(['openssl', *args], check=True, capture_output=True)  # noqa: S603, S607

    openssl(
        'req',
        '-x509',
        '-newkey',
        'rsa:2048',
        '-nodes',
        '-days',
        '2',
        '-subj',
        '/CN=fav test CA',
        '-keyout',
        str(ca_key),
        '-out',
        str(ca_cert),
    )
    openssl('req', '-newkey', 'rsa:2048', '-nodes', '-subj', f'/CN={HOST}', '-keyout', str(key), '-out', str(csr))
    openssl(
        'x509', '-req', '-in', str(csr), '-CA', str(ca_cert), '-CAkey', str(ca_key), '-CAcreateserial',
        '-days', '2', '-extfile', str(ext), '-out', str(cert),
    )  # fmt: skip
    return {'ca': ca_cert, 'cert': cert, 'key': key}


@pytest.fixture
def origin(tls_files: dict[str, Path]) -> Iterator[dict[str, object]]:
    """HTTPS server that records the SNI and Host of each request it serves."""
    record: dict[str, object] = {'sni': [], 'host': []}

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            record['host'].append(self.headers['Host'])  # type: ignore[union-attr]
            body = b'{"ships": []}'
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args: object) -> None:
            return

    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(tls_files['cert'], tls_files['key'])
    context.sni_callback = lambda _sock, name, _ctx: record['sni'].append(name)  # type: ignore[union-attr]
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    record['port'] = server.server_address[1]
    try:
        yield record
    finally:
        server.shutdown()
        server.server_close()


def _pipe(client: socket.socket, upstream: socket.socket, stop: threading.Event) -> None:
    sockets = [client, upstream]
    while not stop.is_set():
        readable, _, _ = select.select(sockets, [], [], 0.2)
        for sock in readable:
            data = sock.recv(65536)
            if not data:
                return
            (upstream if sock is client else client).sendall(data)


def _tunnel(client: socket.socket, targets: list[str], stop: threading.Event) -> None:
    with client:
        head = b''
        while b'\r\n\r\n' not in head:
            chunk = client.recv(4096)
            if not chunk:
                return
            head += chunk
        target = head.split(b' ')[1].decode()
        targets.append(target)
        host, _, port = target.rpartition(':')
        with socket.create_connection((host, int(port))) as upstream:
            client.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
            _pipe(client, upstream, stop)


@pytest.fixture
def connect_proxy() -> Iterator[dict[str, object]]:
    """Plain HTTP CONNECT proxy that records the tunnel targets it was asked for."""
    targets: list[str] = []
    listener = socket.create_server(('127.0.0.1', 0))
    listener.settimeout(0.2)
    stop = threading.Event()

    def serve() -> None:
        while not stop.is_set():
            try:
                client, _ = listener.accept()
            except TimeoutError:
                continue
            threading.Thread(target=_tunnel, args=(client, targets, stop), daemon=True).start()

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    try:
        yield {'url': f'http://127.0.0.1:{listener.getsockname()[1]}', 'targets': targets}
    finally:
        stop.set()
        thread.join()
        listener.close()


@pytest.mark.parametrize('through_proxy', [False, True], ids=['direct', 'proxy'])
def test_tls_is_verified_against_the_original_host(
    through_proxy: bool,  # noqa: FBT001
    tls_files: dict[str, Path],
    origin: dict[str, object],
    connect_proxy: dict[str, object],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv('SSL_CERT_FILE', str(tls_files['ca']))
    proxy = str(connect_proxy['url']) if through_proxy else None
    port = origin['port']
    # The certificate names only l2d.su, so connecting to 127.0.0.1 succeeds only when the
    # handshake is made, and verified, as l2d.su.
    transport = ConnectToTransport(host=HOST, connect_host='127.0.0.1', proxy=proxy, resolves=lambda _host: False)
    with httpx.Client(transport=transport, timeout=10) as client:
        response = client.get(f'https://{HOST}:{port}/data/ships-CN.json')

    assert response.status_code == 200
    assert response.json() == {'ships': []}
    assert origin['sni'] == [HOST]
    assert origin['host'] == [f'{HOST}:{port}']
    assert connect_proxy['targets'] == ([f'127.0.0.1:{port}'] if through_proxy else [])


def test_the_async_transport_verifies_tls_against_the_original_host_through_a_proxy(
    tls_files: dict[str, Path],
    origin: dict[str, object],
    connect_proxy: dict[str, object],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv('SSL_CERT_FILE', str(tls_files['ca']))
    port = origin['port']
    transport = AsyncConnectToTransport(host=HOST, connect_host='127.0.0.1', proxy=str(connect_proxy['url']), resolves=lambda _host: False)

    async def run() -> httpx.Response:
        async with httpx.AsyncClient(transport=transport, timeout=10) as client:
            return await client.get(f'https://{HOST}:{port}/data/ships-CN.json')

    response = asyncio.run(run())

    assert response.status_code == 200
    assert origin['sni'] == [HOST]
    assert connect_proxy['targets'] == [f'127.0.0.1:{port}']


def test_a_certificate_for_another_name_is_still_rejected(
    tls_files: dict[str, Path],
    origin: dict[str, object],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Pointing a different host at the server must fail verification: the swap only ever
    # presents the configured host's name, it does not switch checking off.
    monkeypatch.setenv('SSL_CERT_FILE', str(tls_files['ca']))
    port = origin['port']
    transport = ConnectToTransport(host='other.example', connect_host='127.0.0.1', resolves=lambda _host: False)
    with httpx.Client(transport=transport, timeout=10) as client, pytest.raises(httpx.ConnectError, match='CERTIFICATE_VERIFY_FAILED'):
        client.get(f'https://other.example:{port}/')
