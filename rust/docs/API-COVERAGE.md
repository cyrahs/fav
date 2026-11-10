# API coverage of the key crates (WP0)

The plan flags four replacements as risks: Telethon, Playwright, the `mcp` package and Pillow's animated
webp. WP0 checked each candidate crate against what the Python code actually calls, in compiled code
(`rust/probes/api-coverage`) and with offline tests. Nothing contacted Telegram, a website or a real account.

| Python | Crate (pinned) | Verdict |
| --- | --- | --- |
| Telethon 1.43 | `grammers-client` / `-session` / `-mtsender` / `-tl-types` **0.12.0** | Sufficient, with raw-API fallbacks for two gaps |
| Playwright 1.61 (`uv.lock`) | `chromiumoxide` **0.9.1** (CDP client only) | Sufficient as a CDP client; do not use its launcher |
| `mcp` 1.30 (FastMCP) | `rmcp` **3.9.0** | Sufficient |
| Pillow 11 (animated webp) | `webp-animation` **0.10.0** (bundled static libwebp) + `image` 0.25 | Sufficient |

The probe crate is a workspace member outside `default-members`; CI builds, lints and tests it. It is deleted
when the real implementations land (MCP in WP4, ugoira in WP6, Telegram in WP15, the CDP driver in WP16), and
the pins in `rust/Cargo.toml` move to the crates that use them.

## grammers 0.12.0 (WP15)

Probe: `probes/api-coverage/src/telegram.rs`.

| Python (`src/web/telegram.py`) | grammers | Checked |
| --- | --- | --- |
| `TelegramClient(session_path, api_id, api_hash, ...)` with the existing `.session` file | `SenderPool::with_configuration(Arc<impl Session>, api_id, ConnectionParams)` + `Client::new`. The `Session` trait is ours to implement, and `SessionData` / `DcOption { auth_key }` import a login | Test: a Telethon `sessions` row (dc id, address, port, 256-byte auth key) imports into a grammers session |
| `flood_sleep_threshold` | `ConnectionParams.retry_policy`: a `RetryPolicy` that sleeps through `FLOOD_WAIT` up to the threshold | Test |
| `FloodWaitError.seconds` | `InvocationError::Rpc(RpcError { name: "FLOOD_WAIT", value: Some(seconds) })` | Test |
| `connect()`, `is_user_authorized()` | `Client::is_authorized()`; connections open on demand | Compiles |
| `client.disconnected` | `SenderPoolRunner::run()` returning | Compiles |
| `receive_updates=True, catch_up=True`, `events.NewMessage()` | `Client::stream_updates(updates, UpdatesConfiguration { catch_up: true })`, `Update::NewMessage` | Compiles |
| `events.Album()` | **No equivalent.** Group `NewMessage` updates by `grouped_id` ourselves | Compiles |
| `iter_messages(channel, limit, min_id, reverse=True, wait_time)` | Telethon never sends `min_id`: in reverse it pages upward from `offset_id = min_id + 1` with a negative `add_offset` and drops ids `<= min_id` client-side. grammers' `iter_messages(peer).reverse(true).offset_id(min_id)` pages the same way (its request `min_id` is the exclusive offset). `wait_time` is our own sleep between pages | Compiles |
| `iter_messages(channel, limit, min_id, reverse=False)` (newest window) | `iter_messages(peer).limit(n)`, stopping at ids `<= min_id` ourselves, as Telethon does | Compiles |
| `get_entity(PeerChannel(id))`, `get_messages(channel, ids=...)` | `Session::peer_ref` (access hash from the cache) + `Client::resolve_peer`, `Client::get_messages_by_id` | Compiles |
| `MessageMediaPhoto`, `DocumentAttributeVideo`, sticker / custom-emoji attributes, `mime_type`, `grouped_id` | `Media::{Photo, Document, Sticker}`, `Document::mime_type()`, the raw `tl` attributes, `Message::grouped_id()` | Compiles |
| `download_media(file=..., progress_callback=...)` | `Client::iter_download` chunk by chunk (progress is ours). `Client::download_media` has no progress callback but downloads big files over parallel workers | Compiles |

Notes for WP15:

- The probe's login test only round-trips the values of a `sessions` row into grammers; it does not read a
  `.session` file. WP15 reads real files (generate one with Telethon in a golden) and also imports the
  `entities` table: `get_entity(PeerChannel(id))` needs the access hash stored there. `server_address` can be
  IPv6 in Telethon; the probe's struct is IPv4 only.
- Telethon retries a request up to `request_retries` (default 5) and sleeps through every `FLOOD_WAIT` under
  the threshold; the probe's policy retries once. Copy Telethon's counts.
- Session file. Telethon's `.session` is SQLite: `sessions(dc_id, server_address, port, auth_key,
  takeout_id, tmp_auth_key)`, `entities(id, hash, username, phone, name, date)` with marked ids (`-100…` for
  channels), `update_state(id, pts, qts, date, seq)`, `sent_files`, `version`. Reading it needs an SQLite
  driver (sqlx's `sqlite` feature fits the stack). Decide in WP15 whether the Rust build keeps writing
  Telethon's format (true drop-in, both directions) or imports once into its own file; record the choice in
  `DIVERGENCES.md`.
- `FLOOD_PREMIUM_WAIT_X` is a separate Telethon error that is not a `FloodWaitError` subclass in 1.43;
  match Python, which only cools down on `FLOOD_WAIT`.
- Reconnection is ours: when the runner returns, build a new pool on the same session, as the Python loop does.
- grammers' proxy support is SOCKS5 only (feature `proxy`). The Python client uses no proxy.
- The album debounce has to reproduce Telethon's `events.Album` timing closely enough that the
  `test_telegram.py` album tests pass; that is the most likely place for a divergence.

## chromiumoxide 0.9.1 (WP16, WP17)

Probe: `probes/api-coverage/src/cdp.rs` until WP16, which deleted it (it is still in the history: `git show
6165a61:rust/probes/api-coverage/src/cdp.rs`). Two tests drove a real headless Chromium against loopback servers
(`FAV_TEST_CHROMIUM`; CI points it at the runner's Chrome, and a missing binary fails in CI rather than skipping).
The "Checked" column is what the probe checked. Since WP16, `fav-browser/src/driver/tests.rs` covers every row;
WP17 added the network listeners and `Network.getResponseBody` (`Page::network_events`, `Page::response_body`,
tested there and end to end by `rednote/note_browser/tests.rs`, which walks a local likes list in real Chromium),
and `chromium.launch` without a profile for the Nikke capture (`BrowserContext::launch_browser`). URL-filtered cookies are `Storage.getCookies` plus Playwright's own filter (`driver::filter_cookies`),
not `Network.getCookies`.

| Python (`rednote_browser.py`, `nikke_runtime.py`, `browser_login.py`) | chromiumoxide | Checked |
| --- | --- | --- |
| `launch_persistent_context(...)`, `chromium.launch(...)` | **Spawn Chromium ourselves** with an exact argv ending in `--remote-debugging-port=0`, read `DevTools listening on ws://…` from stderr, then `Browser::connect` | Test |
| Plain Chromium with a DevTools port (login window) | `Browser::connect(websocket_url)` | Compiles |
| `add_init_script` | `Page.addScriptToEvaluateOnNewDocument` | Test |
| `present_mac_user_agent` (`Emulation.setUserAgentOverride` with client hints) | `SetUserAgentOverrideParams` + `UserAgentMetadata` | Test (`navigator.userAgentData.platform`) |
| `locale`, `timezone_id`, `viewport` | `Emulation.setLocaleOverride`, `setTimezoneOverride`, `setDeviceMetricsOverride` | Test (timezone) |
| `page.on('request' / 'response' / 'requestfinished' / 'requestfailed')`, `response.json()` | `Network.requestWillBeSent` / `responseReceived` / `loadingFinished` / `loadingFailed` streams, `Network.getResponseBody` | Test |
| `goto(url, wait_until='domcontentloaded')` | `Page.navigate` + `Page.domContentEventFired` | Test |
| `page.evaluate(...)`, `wait_for_function(...)` | `Page::evaluate`; `wait_for_function` becomes a polling loop | Test (evaluate) |
| `locator('.reds-tab-item', has_text=label).first.click()`, `locator(css).click()` | `Page::find_elements` + `Element::inner_text` filter + `scroll_into_view` + `Element::click`; no match is an error | Test (click lands; missing tab errors) |
| `get_by_text(title, exact=True).last.click()` (Nikke runtime) | Same building blocks with an exact text match on the last element | Compiles (shares the click path) |
| `mouse.move(x, y)`, `mouse.wheel(0, dy)`, `mouse.click(x, y)` | `Page::move_mouse`, `Input.dispatchMouseEvent { type: mouseWheel }`, `Page::click(Point)` | Test (move + wheel scrolls) |
| `page.reload(wait_until='domcontentloaded')` | `Page::reload` | Test |
| `context.cookies()` / `Storage.getCookies` | `Storage.getCookies` through `Browser::execute` | Test |
| `context.cookies([WEB_ORIGIN])` (URL-filtered) | `Network.getCookies { urls }` | Test (path-scoped cookie only for its URL) |
| `proxy={'server', 'username', 'password'}` | `--proxy-server` + `Page::authenticate` (answers `Fetch.authRequired`) | Test (loopback proxy demanding Basic auth) |

Notes for WP16:

- Do not launch through `BrowserConfig`: it stores arguments in a `HashMap` (random order), always adds
  `--disable-extensions`, adds its own `--headless`/`--no-sandbox` sets, and joins repeated keys with commas.
  Copying Playwright's argument list exactly (the plan's requirement) is only possible by spawning Chromium
  ourselves; `launch_argv` in the probe shows the shape. Copy the full default list from the Playwright
  release in `uv.lock`, minus `--enable-automation`.
- Playwright's click waits for actionability (attached, visible, stable, enabled, receives the pointer at
  the click point) and retries until its timeout; `Element::click` does none of that. WP16 adds those waits
  and the hit test, because `rednote_browser.py` relies on a click either landing or failing loudly.
- The headed login window (`browser_login.py`, `channel='chromium'`, `headless=False` on Xvnc's display) is
  the same spawn path without `--headless` and with `DISPLAY` set; not exercised here (no X server in CI).
- `HeadlessMode` is not re-exported (`chromiumoxide::browser::config` is private); irrelevant once we spawn.
- `navigator.webdriver` is absent from the returned value when `undefined`; evaluate a boolean instead.
- The CDP types are generated from a fixed protocol revision. Events Chromium sends that the revision does
  not know are dropped, not fatal.

## rmcp 3.9.0 (WP4)

Probe: `probes/api-coverage/src/mcp.rs`. Tests send JSON-RPC to the axum router in process.

| Python (`src/api/mcp_server.py`) | rmcp | Checked |
| --- | --- | --- |
| `FastMCP(name='fav', instructions=...)` | `ServerConfig::new(..).with_server_info(Implementation::new("fav", ..)).with_instructions(..)` | Test |
| `stateless_http=True, json_response=True` | `StreamableHttpServerConfig { legacy_session_mode: false, json_response: true }` with `NeverSessionManager` | Test (`application/json` answer, no session) |
| DNS-rebinding protection off | `StreamableHttpServerConfig::disable_allowed_hosts()` | Test (in-cluster `Host`) |
| Mounted at `/mcp` on the API app | `StreamableHttpService` is a tower service: `Router::nest_service("/mcp", ..)` | Test |
| `@mcp.tool()` input schemas | Hand-built `Tool` with the schema as a JSON object, passed through verbatim | Test (byte-identical schema, key order kept) |
| Tool results, `ToolError` | `CallToolResult::structured` / `CallToolResult::error` | Test |

Outcome (WP4): the endpoint does not use `rmcp`. Matching FastMCP's answers to malformed requests (its
error envelopes, pydantic's validation texts, the `-32602` with empty data, the 4 MiB limit) needed overrides
at every layer, so `fav-api/src/mcp` implements the stateless JSON transport directly and golden-checks it
byte for byte (`DIVERGENCES.md` D31). The pin and the probe stay as the record of this check.

Notes for WP4:

- Key order in schemas survives only with `serde_json`'s `preserve_order` feature. Cargo unifies features,
  so it is effectively on workspace-wide once any crate enables it; keep it on (`CONVENTIONS.md`).
- rmcp's newest protocol version is `2026-07-28`; Python's `mcp` 1.30 supports up to `2025-11-25`
  (`2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25`). Override
  `ServerHandler::supported_protocol_versions` to Python's list so LiteLLM negotiates the same version.
- Golden the exact `tools/list` and `initialize` responses from FastMCP and diff, including `isError`,
  `structuredContent` and `outputSchema`, which FastMCP derives from return annotations.

## webp-animation 0.10.0 (WP6)

Probe: `probes/api-coverage/src/webp.rs` until WP6, which deleted it; the check now lives in
`fav-sources/src/pixiv.rs` (`synthesize_ugoira_webp`) and its tests.

| Python (`src/web/pixiv.py::synthesize_ugoira_webp`) | Rust | Checked |
| --- | --- | --- |
| `Image.open(frame)` (JPEG or PNG) | `image::load_from_memory(..).to_rgba8()` | Test |
| `save(save_all=True, append_images=..., duration=[...], loop=0, quality=90, method=4)` | `Encoder::new_with_options` (`loop_count: 0`, lossy, `quality: 90.0`, `method: 4`), `add_frame(frame, start_ms)`, `finalize(end_ms)` | Test: decoded frame timestamps match the delays |

Notes for WP6: the output is not byte-identical to Pillow (the plan accepts that); compare frame count,
dimensions and per-frame durations by decoding both with `webp_animation::Decoder`. The `static` feature
builds libwebp from source, so the image needs no system libwebp.

Outcome (WP6): ugoira encode with `kmin 3`, `kmax 5`, `allow_mixed false` and `minimize_size false` as
Pillow passes them; frames are decoded one at a time. Tests decode the result with `webp_animation::Decoder`
and check frame count and end times; a single frame comes out as a still image, as Pillow's `_save` path
writes it. The bytes and the background colour metadata differ (`DIVERGENCES.md` D51).
