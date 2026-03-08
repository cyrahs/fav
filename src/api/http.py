from __future__ import annotations

import logging
from http.server import BaseHTTPRequestHandler

from .constants import _HEADER_CONTENT_LENGTH
from .service import FavApiService

log = logging.getLogger('fav-api')


class FavApiRequestHandler(BaseHTTPRequestHandler):
    service: FavApiService
    protocol_version = 'HTTP/1.1'
    server_version = 'FavAPI/1.0'

    def _read_request_body(self) -> bytes | None:
        content_length = self.headers.get(_HEADER_CONTENT_LENGTH)
        if not content_length:
            return None
        try:
            length = int(content_length)
        except ValueError:
            return None
        if length <= 0:
            return None
        return self.rfile.read(length)

    def _send(self) -> None:
        response = self.service.handle_request(
            method=self.command,
            path=self.path,
            headers=dict(self.headers.items()),
            body=self._read_request_body(),
        )
        body = response.body or b''
        self.send_response(response.status.value)
        for key, value in response.headers.items():
            self.send_header(key, value)
        self.send_header(_HEADER_CONTENT_LENGTH, str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_DELETE(self) -> None:
        self._send()

    def do_GET(self) -> None:
        self._send()

    def do_PATCH(self) -> None:
        self._send()

    def do_POST(self) -> None:
        self._send()

    def do_PUT(self) -> None:
        self._send()

    def log_message(self, fmt: str, *args: object) -> None:
        log.info('%s - - %s', self.address_string(), fmt % args)


def build_handler(service: FavApiService) -> type[FavApiRequestHandler]:
    class _BoundHandler(FavApiRequestHandler):
        pass

    _BoundHandler.service = service
    return _BoundHandler
