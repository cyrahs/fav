from __future__ import annotations

import base64
import json
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING

import httpx
from Crypto.Cipher import AES
from Crypto.Hash import MD5
from Crypto.Util.Padding import unpad

from src.core import logger

if TYPE_CHECKING:
    from collections.abc import Sequence

log = logger.get('cookiecloud')


@dataclass(frozen=True, slots=True)
class CookieProfile:
    """Where one site's cookies sit in a vault, and which of them it cannot work without."""

    domains: tuple[str, ...]
    required_cookies: tuple[str, ...]


# The cookies src/web/bilibili.py needs to build a Credential.
BILIBILI_PROFILE = CookieProfile(
    domains=('bilibili.com',),
    required_cookies=('sessdata', 'bili_jct', 'buvid3', 'dedeuserid'),
)
# X serves one session under both hostnames; which one a vault carries depends on
# where the browser was when the extension last synced.
TWITTER_PROFILE = CookieProfile(
    domains=('x.com', 'twitter.com'),
    required_cookies=('auth_token', 'ct0'),
)
# The ajax API authenticates with the single session cookie; probe() lower-cases
# cookie names before matching, so this is 'phpsessid'. Which hostname a vault
# files it under depends on the browser, hence both.
PIXIV_PROFILE = CookieProfile(
    domains=('pixiv.net', 'www.pixiv.net'),
    required_cookies=('phpsessid',),
)
PROFILES: dict[str, CookieProfile] = {
    'bilibili': BILIBILI_PROFILE,
    'twitter': TWITTER_PROFILE,
    'pixiv': PIXIV_PROFILE,
}


class CookieCloudClient:
    def __init__(self, server_url: str, uuid: str, password: str, user_agent: str | None = None, proxy: str | None = None) -> None:
        """Initialize the CookieCloud client.

        Args:
            server_url (str): The URL of the CookieCloud server
            uuid (str): Your CookieCloud UUID
            password (str): Your CookieCloud password
            user_agent (str, optional): Custom user agent for requests

        """
        self.server_url = server_url.rstrip('/')
        self.uuid = uuid
        self.password = password
        self.user_agent = user_agent or 'CookieCloudClient/Python'
        self.client = httpx.Client(proxy=proxy, timeout=10, headers={'User-Agent': self.user_agent})
        self.key = MD5.new(f'{self.uuid}-{self.password}'.encode()).hexdigest()[:16].encode()  # noqa: S303

    def _decrypt_data(self, encrypted_text: str) -> str:
        # Decode the base64 encoded encrypted text
        encrypted_bytes = base64.b64decode(encrypted_text)

        if encrypted_bytes[:8] != b'Salted__':
            msg = 'Invalid OpenSSL encrypted text'
            raise ValueError(msg)

        salt = encrypted_bytes[8:16]

        # OpenSSL key derivation
        key_iv = b''
        prev = b''
        while len(key_iv) < 48:  # We need 32 bytes for key and 16 bytes for IV  # noqa: PLR2004
            prev = MD5.new(prev + self.key + salt).digest()  # noqa: S303
            key_iv += prev

        derived_key = key_iv[:32]  # Use first 32 bytes for the key
        iv = key_iv[32:48]  # Use next 16 bytes for the IV
        ciphertext = encrypted_bytes[16:]

        cipher = AES.new(derived_key, AES.MODE_CBC, iv)
        decrypted_bytes = cipher.decrypt(ciphertext)
        decrypted_bytes = unpad(decrypted_bytes, AES.block_size)

        # Convert the decrypted bytes to a string

        return decrypted_bytes.decode()

    def get_cookies(self) -> dict[str, list[dict]]:
        """Fetch and decrypt cookies from CookieCloud server.

        Returns:
            dict: Dictionary of cookies organized by domain

        """
        url = f'{self.server_url}/get/{self.uuid}'
        headers = {'User-Agent': self.user_agent}

        try:
            response = self.client.get(url, headers=headers)
            response.raise_for_status()
            response_data = response.json()
            return json.loads(self._decrypt_data(response_data['encrypted']))['cookie_data']

        except httpx.RequestError as e:
            log.exception('Request error')
            msg = f'Failed to connect to CookieCloud server: {e}'
            raise ConnectionError(msg) from e

    def save_to_netscape_format(self, domain: str | Sequence[str], output_path: str | Path) -> None:
        """Save cookies for one or more domains to Netscape cookie.txt format.

        Args:
            domain (str or sequence of str): The domain(s) to extract cookies for
                (e.g., 'bilibili.com', or ('x.com', 'twitter.com')). Domains the
                vault does not carry are skipped; it is an error only when none
                of them are present.
            output_path (str or Path): Path where to save the cookie file

        """
        if isinstance(output_path, str):
            output_path = Path(output_path)
        domains = (domain,) if isinstance(domain, str) else tuple(domain)

        cookies = self.get_cookies()
        matched = [(name, cookies[name]) for name in domains if cookies.get(name)]
        if not matched:
            msg = f'No cookies found for domain: {", ".join(domains)}'
            raise ValueError(msg)

        cookie_content = [
            '# Netscape HTTP Cookie File',
            '# https://curl.se/docs/http-cookies.html',
            '# This file was generated by CookieCloud',
        ]
        for matched_domain, domain_cookies in matched:
            for cookie in domain_cookies:
                secure = 'TRUE' if cookie.get('secure', False) else 'FALSE'
                host_only = 'TRUE' if not cookie.get('hostOnly', True) else 'FALSE'  # set Include Subdomains
                expiry = cookie.get('expirationDate', int(time.time() + 157680000))  # Default: now + 5 years
                line = f'{cookie.get("domain", "." + matched_domain)}\t'
                line += f'{host_only}\t'
                line += f'{cookie.get("path", "/")}\t'
                line += f'{secure}\t'
                line += f'{int(expiry)}\t'
                line += f'{cookie["name"]}\t'
                line += f'{cookie["value"]}'
                cookie_content.append(line)

        output_path.write_text('\n'.join(cookie_content))


@dataclass(frozen=True, slots=True)
class CookieCloudProbe:
    """Outcome of a connectivity/credential check against a CookieCloud server."""

    ok: bool
    code: str
    message: str
    domain_count: int = 0
    domain_cookie_count: int = 0
    missing_cookies: tuple[str, ...] = field(default=())


def _fetch_for_probe(server_url: str, uuid: str, password: str) -> tuple[dict[str, list[dict]] | None, CookieCloudProbe | None]:
    """Return (cookies, None) on success or (None, failure) describing what went wrong."""
    client = CookieCloudClient(server_url, uuid, password)
    try:
        return (client.get_cookies(), None)
    except ConnectionError as exc:
        return (None, CookieCloudProbe(ok=False, code='unreachable', message=str(exc)))
    except httpx.HTTPStatusError as exc:
        return (
            None,
            CookieCloudProbe(
                ok=False,
                code='http_error',
                message=f'CookieCloud server returned HTTP {exc.response.status_code}',
            ),
        )
    except (ValueError, KeyError) as exc:
        # unpad/base64/JSON all fail this way when the password is wrong.
        log.debug('CookieCloud decrypt failed: %s', exc)
        return (
            None,
            CookieCloudProbe(
                ok=False,
                code='decrypt_failed',
                message='Could not decrypt the vault. Check the UUID and password.',
            ),
        )
    except Exception as exc:  # noqa: BLE001
        log.warning('CookieCloud probe failed: %s', exc)
        return (None, CookieCloudProbe(ok=False, code='error', message=str(exc)))
    finally:
        client.client.close()


def probe(server_url: str, uuid: str, password: str, *, profile: CookieProfile | None = BILIBILI_PROFILE) -> CookieCloudProbe:
    """Fetch and decrypt a CookieCloud vault, reporting why it is unusable if it is.

    Distinguishes the failure modes an operator actually needs to tell apart: the
    server being unreachable, the password being wrong (decryption fails), and the
    vault simply not carrying the cookies this deployment needs. ``profile=None``
    checks reachability and decryption only -- used for the shared credential,
    which is not tied to any one source's cookies.
    """
    missing_config = [name for name, value in (('server_url', server_url), ('uuid', uuid), ('password', password)) if not value.strip()]
    if missing_config:
        return CookieCloudProbe(
            ok=False,
            code='incomplete',
            message=f'Missing: {", ".join(missing_config)}',
        )

    cookies, failure = _fetch_for_probe(server_url, uuid, password)
    if failure is not None:
        return failure
    assert cookies is not None  # noqa: S101 - _fetch_for_probe returns one or the other

    domain_count = len(cookies)
    if profile is None:
        return CookieCloudProbe(
            ok=True,
            code='ok',
            message=f'OK — vault decrypted, {domain_count} domains.',
            domain_count=domain_count,
        )

    domain_label = ' / '.join(profile.domains)
    # A profile may list several hostnames for the same session; take every one the vault has.
    domain_cookies = [cookie for name in profile.domains for cookie in (cookies.get(name) or [])]
    if not domain_cookies:
        return CookieCloudProbe(
            ok=False,
            code='no_domain_cookies',
            message=f'Vault decrypted ({domain_count} domains) but has no cookies for {domain_label}.',
            domain_count=domain_count,
        )

    present = {str(cookie.get('name', '')).lower() for cookie in domain_cookies if isinstance(cookie, dict)}
    missing = tuple(name for name in profile.required_cookies if name not in present)
    if missing:
        return CookieCloudProbe(
            ok=False,
            code='missing_cookies',
            message=f'{domain_label} cookies are present but incomplete; missing {", ".join(missing)}. Re-sync the browser extension.',
            domain_count=domain_count,
            domain_cookie_count=len(domain_cookies),
            missing_cookies=missing,
        )

    return CookieCloudProbe(
        ok=True,
        code='ok',
        message=f'OK — {domain_count} domains, {len(domain_cookies)} cookies for {domain_label}.',
        domain_count=domain_count,
        domain_cookie_count=len(domain_cookies),
    )
