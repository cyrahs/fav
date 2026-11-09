# API coverage of the key crates (WP0)

The plan flags four replacements as risks: Telethon, Playwright, the `mcp` package and Pillow's animated
webp. WP0 checked each candidate crate against what the Python code actually calls, in compiled code
(`rust/probes/api-coverage`) and with offline tests. Nothing contacted Telegram, a website or a real account.

| Python | Crate (pinned) | Verdict |
| --- | --- | --- |
| Telethon 1.43 | `grammers-client` / `-session` / `-mtsender` / `-tl-types` **0.12.0** | Sufficient, with raw-API fallbacks for two gaps |
| Playwright 1.59 | `chromiumoxide` **0.9.1** (CDP client only) | Sufficient as a CDP client; do not use its launcher |
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
| `iter_messages(channel, limit, min_id, reverse, wait_time)` | `MessageIter` has `limit`, `reverse`, `offset_id` but **no `min_id`**: use raw `messages.getHistory { min_id, .. }` through `Client::invoke`. `wait_time` is our own sleep between pages | Compiles |
| `get_entity(PeerChannel(id))`, `get_messages(channel, ids=...)` | `Session::peer_ref` (access hash from the cache) + `Client::resolve_peer`, `Client::get_messages_by_id` | Compiles |
| `MessageMediaPhoto`, `DocumentAttributeVideo`, sticker / custom-emoji attributes, `mime_type`, `grouped_id` | `Media::{Photo, Document, Sticker}`, `Document::mime_type()`, the raw `tl` attributes, `Message::grouped_id()` | Compiles |
| `download_media(file=..., progress_callback=...)` | `Client::iter_download` chunk by chunk (progress is ours). `Client::download_media` has no progress callback but downloads big files over parallel workers | Compiles |

Notes for WP15:

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

Probe: `probes/api-coverage/src/cdp.rs`. Two tests drive a real headless Chromium against loopback servers
(`FAV_TEST_CHROMIUM`; CI points it at the runner's Chrome, and a missing binary fails in CI rather than skipping).

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
| `context.cookies(...)` / `Storage.getCookies` | `Storage.getCookies` through `Browser::execute` | Test |
| `proxy={'server', 'username', 'password'}` | `--proxy-server` + `Page::authenticate` (answers `Fetch.authRequired`) | Test (loopback proxy demanding Basic auth) |

Notes for WP16:

- Do not launch through `BrowserConfig`: it stores arguments in a `HashMap` (random order), always adds
  `--disable-extensions`, adds its own `--headless`/`--no-sandbox` sets, and joins repeated keys with commas.
  Copying Playwright's argument list exactly (the plan's requirement) is only possible by spawning Chromium
  ourselves; `launch_argv` in the probe shows the shape. Copy the full default list from the Playwright
  release in `uv.lock`, minus `--enable-automation`.
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

Notes for WP4:

- Key order in schemas survives only with `serde_json`'s `preserve_order` feature. Cargo unifies features,
  so it is effectively on workspace-wide once any crate enables it; keep it on (`CONVENTIONS.md`).
- rmcp's newest protocol version is `2026-07-28`; Python's `mcp` 1.30 supports up to `2025-11-25`
  (`2024-11-05`, `2025-03-26`, `2025-06-18`, `2025-11-25`). Override
  `ServerHandler::supported_protocol_versions` to Python's list so LiteLLM negotiates the same version.
- Golden the exact `tools/list` and `initialize` responses from FastMCP and diff, including `isError`,
  `structuredContent` and `outputSchema`, which FastMCP derives from return annotations.

## webp-animation 0.10.0 (WP6)

Probe: `probes/api-coverage/src/webp.rs`.

| Python (`src/web/pixiv.py::synthesize_ugoira_webp`) | Rust | Checked |
| --- | --- | --- |
| `Image.open(frame)` (JPEG or PNG) | `image::load_from_memory(..).to_rgba8()` | Test |
| `save(save_all=True, append_images=..., duration=[...], loop=0, quality=90, method=4)` | `Encoder::new_with_options` (`loop_count: 0`, lossy, `quality: 90.0`, `method: 4`), `add_frame(frame, start_ms)`, `finalize(end_ms)` | Test: decoded frame timestamps match the delays |

Notes for WP6: the output is not byte-identical to Pillow (the plan accepts that); compare frame count,
dimensions and per-frame durations by decoding both with `webp_animation::Decoder`. The `static` feature
builds libwebp from source, so the image needs no system libwebp.
