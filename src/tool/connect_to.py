"""``curl --connect-to`` for httpx: reach a host whose own DNS record is gone.

When a site behind a CDN loses its DNS record, the CDN usually keeps serving it: the edge still
routes by TLS SNI and ``Host``, so a connection opened to another address of the same zone (a
sibling hostname that still resolves, or one of the zone's edge IPs) reaches it fine. Only the
connection target changes; the TLS handshake, certificate check and ``Host`` header stay those of
the original host.

That cannot be done by resolving the name to an address locally (``curl --resolve``), because
through an HTTP proxy the CONNECT target is a hostname the proxy resolves itself, and it fails the
same way. So requests are rewritten to target the substitute host, and the TLS context swaps the
server name back to the original host when the handshake is made. That works the same through a
proxy tunnel and on a direct connection.

The substitute is used only while the host does not resolve, so a restored record takes over
again on its own. The check runs once per transport, which the crawler opens once per run.
"""

from __future__ import annotations

import asyncio
import os
import socket
import ssl
from typing import TYPE_CHECKING, Any

import certifi
import httpx

from src.core import logger

if TYPE_CHECKING:
    from collections.abc import Callable

log = logger.get('connect_to')

_HTTPS_PORT = 443


def host_resolves(host: str) -> bool:
    try:
        return bool(socket.getaddrinfo(host, _HTTPS_PORT, type=socket.SOCK_STREAM))
    except (OSError, UnicodeError):
        return False


class _ServerNameContext(ssl.SSLContext):
    """Present ``server_name`` (SNI and certificate hostname) on connections made to ``connect_host``.

    httpcore takes the TLS server name from the URL it connects to, and ignores the
    ``sni_hostname`` request extension inside a proxy tunnel, so the swap is done here instead.
    """

    connect_host: str
    server_name: str

    def _server_hostname(self, server_hostname: str | None) -> str | None:
        return self.server_name if server_hostname == self.connect_host else server_hostname

    def wrap_socket(self, sock: socket.socket, *args: Any, server_hostname: str | None = None, **kwargs: Any) -> ssl.SSLSocket:
        return super().wrap_socket(sock, *args, server_hostname=self._server_hostname(server_hostname), **kwargs)

    def wrap_bio(
        self, incoming: ssl.MemoryBIO, outgoing: ssl.MemoryBIO, *args: Any, server_hostname: str | None = None, **kwargs: Any
    ) -> ssl.SSLObject:
        return super().wrap_bio(incoming, outgoing, *args, server_hostname=self._server_hostname(server_hostname), **kwargs)


def _server_name_context(*, connect_host: str, server_name: str) -> ssl.SSLContext:
    # Same trust roots httpx picks for ``verify=True``; verification stays fully on.
    context = _ServerNameContext(ssl.PROTOCOL_TLS_CLIENT)
    if os.environ.get('SSL_CERT_FILE'):
        context.load_verify_locations(cafile=os.environ['SSL_CERT_FILE'])
    elif os.environ.get('SSL_CERT_DIR'):
        context.load_verify_locations(capath=os.environ['SSL_CERT_DIR'])
    else:
        context.load_verify_locations(cafile=certifi.where())
    context.connect_host = connect_host
    context.server_name = server_name
    return context


def _redirected(request: httpx.Request, connect_host: str) -> httpx.Request:
    # The original headers carry ``Host`` already, and a request built from a stream does not
    # recompute it, so the origin still sees its own name.
    return httpx.Request(
        request.method,
        request.url.copy_with(host=connect_host),
        headers=request.headers,
        stream=request.stream,
        extensions=request.extensions,
    )


def _log_redirect(host: str, connect_host: str) -> None:
    log.warning('%s does not resolve; connecting through %s instead', host, connect_host)


class ConnectToTransport(httpx.BaseTransport):
    def __init__(
        self,
        *,
        host: str,
        connect_host: str,
        proxy: str | None = None,
        limits: httpx.Limits | None = None,
        resolves: Callable[[str], bool] = host_resolves,
    ) -> None:
        self._host = host
        self._connect_host = connect_host
        self._resolves = resolves
        self._redirect: bool | None = None
        context = _server_name_context(connect_host=connect_host, server_name=host)
        self._inner = httpx.HTTPTransport(verify=context, proxy=proxy or None, limits=limits or httpx.Limits())

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        if request.url.host == self._host:
            if self._redirect is None:
                self._redirect = not self._resolves(self._host)
                if self._redirect:
                    _log_redirect(self._host, self._connect_host)
            if self._redirect:
                request = _redirected(request, self._connect_host)
        return self._inner.handle_request(request)

    def close(self) -> None:
        self._inner.close()


class AsyncConnectToTransport(httpx.AsyncBaseTransport):
    def __init__(
        self,
        *,
        host: str,
        connect_host: str,
        proxy: str | None = None,
        limits: httpx.Limits | None = None,
        resolves: Callable[[str], bool] = host_resolves,
    ) -> None:
        self._host = host
        self._connect_host = connect_host
        self._resolves = resolves
        self._redirect: bool | None = None
        context = _server_name_context(connect_host=connect_host, server_name=host)
        self._inner = httpx.AsyncHTTPTransport(verify=context, proxy=proxy or None, limits=limits or httpx.Limits())

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        if request.url.host == self._host:
            if self._redirect is None:
                self._redirect = not await asyncio.to_thread(self._resolves, self._host)
                if self._redirect:
                    _log_redirect(self._host, self._connect_host)
            if self._redirect:
                request = _redirected(request, self._connect_host)
        return await self._inner.handle_async_request(request)

    async def aclose(self) -> None:
        await self._inner.aclose()
