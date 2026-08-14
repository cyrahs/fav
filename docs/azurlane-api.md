# Azur Lane API Reference

Contract for the fav Azur Lane collection API, for building a frontend that mirrors l2d.su.
Data flows in through the crawler (`src/web/azurlane.py`); everything below describes what is
served after at least one crawl run.

## Authentication and base URL

All JSON endpoints live under `/api/v2` and require `Authorization: Bearer <API_TOKEN>`.
Static asset files under `/static/azurlane/` are served by the nginx sidecar directly from the
collection volume and carry no auth.

## Endpoints

### `GET /api/v2/azurlane/characters`

Paged character list. Query params: `query` (matches zh/en names and character key),
`limit` (default 200), `offset`. Response:

```json
{"items": [CharacterSummary], "total": 882, "limit": 200, "offset": 0}
```

### `GET /api/v2/azurlane/sidebar/characters`

Lightweight list for navigation, sorted by freshness, ETag-cached (send `If-None-Match`,
handle 304). Same `query` param, no paging. Carries `icon` and `source_metadata` so the sidebar
can render avatars and filters without fetching the full list.

### `GET /api/v2/azurlane/characters/{character_key}`

Full character detail. Extends CharacterSummary with:

- `models` — every catalog entry for this character, sorted by `(type, costume.key, model_id)`
- `live2d_models` / `spine_models` / `painting_models` — the same entries filtered by type
- `assets` — flat list of every asset of every model

### `GET /api/v2/azurlane/characters/{character_key}/ship-detail`

Raw l2d.su per-ship detail payload (CN region), passed through unmodified. 404 with code
`azurlane_ship_detail_not_found` until the detail crawl has fetched that ship. The same JSON
is also written to `detail.json` in the character directory (see Static files).

### `GET /api/v2/azurlane/skin-updates`

The source's own "what changed" feed, for a skin-updates panel. ETag + `public, max-age=300`,
same as the sidebar; send `If-None-Match` to get a 304.

```json
{
  "game_version": "9.7.323",
  "region": "CN",
  "generated_at": "2026-08-13T05:33:57.726Z",
  "new_skins": [
    {"ship_group_id": 10720, "ship_name": "本宁顿", "skin_name": "全速！盛夏逐光企划",
     "skin_type": "Live2D+", "skin_ids": [107201]}
  ],
  "skin_update_history": [
    {"date": "2026-08-13T03:57:01.516Z", "version": "9.7.323", "skins": [ ...same shape... ]}
  ]
}
```

Both arrays carry the same entry shape. The archived snapshot stores `new_skins` in snake_case
and history entries in the source's camelCase; this endpoint reads either and always emits
snake_case. 404 with code `azurlane_skin_updates_not_found` before the first crawl has written
a snapshot.

Do not read `_source/l2d-su-snapshot.json` for this — it is not served over HTTP, and it is
megabytes of catalog and filter enumerations for a payload of a few kilobytes.

## Data model

### CharacterSummary

```json
{
  "character_key": "biaoqiang",
  "source_id": 1,
  "title": "biaoqiang - Javelin",
  "display_name": "Javelin",
  "name_zh": "标枪",
  "name_en": "Javelin",
  "directory_name": "biaoqiang - Javelin",
  "source_metadata": {
    "model_ids": ["azurlane:live2d:biaoqiang:biaoqiang_2", "azurlane:painting:biaoqiang:biaoqiang"],
    "sources": ["l2d.su"],
    "nation": "Royal Navy",
    "ship_type": "Destroyer",
    "rarity": "SR",
    "class_name": "J Class",
    "default_skin_id": 10,
    "skin_series": ["Default", "Swimsuits"]
  },
  "model_counts": {"live2d": 1, "spine": 0, "painting": 3, "total": 4},
  "asset_counts": {"painting.image": 3, "voice.audio": 42},
  "representative_asset": {},
  "icon": {},
  "fetched_at": "…", "completed_at": "…"
}
```

`source_metadata.nation` / `ship_type` / `rarity` are the l2d.su localized names from the CN
index and drive the list filters (the full enumeration tables from the index are stored in
`_source/l2d-su-snapshot.json` under `filters`).

**Filter dimensions available at list level:**

- `nation`, `ship_type`, `rarity` — straight from the index.
- `skin_series` — deduplicated list of the ship's skin series labels, preserving skin order.
  Each label is the skin's `shopTypeName`, falling back to its `skinTypeName` for skins with no
  shop series; that is exactly how l2d.su builds its own `filters.skinSeries` enumeration, so
  these values always match entries in that list.
- `class_name` — **detail-gated**. The ship index does not carry the ship class at all, so this
  field only appears once that ship's detail payload has been fetched and stored (see Freshness
  semantics). Treat it as optional and hide the class filter for ships that lack it, rather
  than assuming an empty value means "no class". The index's `filters.classes` enumeration is a
  flat list of class names with no ship mapping, so it cannot be used to fill the gap.

`icon` is the square icon of the ship's default skin (`default_skin_id`), shaped like any other
asset object — use it for list and sidebar avatars. `representative_asset` still exists but
prefers a full model texture, which makes a poor avatar. If the default skin's icon has not been
downloaded, `icon` falls back to any available square icon for the ship, and is `null` when the
ship has none yet.

### Model entry

`type` is one of `live2d`, `spine`, `painting`. Every skin of every ship has exactly one
`painting` entry (the static art view); skins with a dynamic model additionally have a
`live2d` or `spine` entry sharing the same `costume.id`. Model IDs follow
`azurlane:{type}:{character_key}:{costume_key}`.

```json
{
  "model_id": "azurlane:painting:biaoqiang:biaoqiang_2",
  "type": "painting",
  "source": "l2d.su",
  "costume": {"key": "biaoqiang_2", "id": 11, "name_zh": "默认", "name_en": "Default"},
  "source_urls": {"primary": "https://static.l2d.su/azurlane/painting/biaoqiang_2.webp", "fallback": "", "display_info": ""},
  "availability": {"source_state": "unchecked", "archive_state": "complete", "asset_status_counts": {"downloaded": 9}},
  "source_metadata": {"costume": {"skin_type": "Skin", "feature_tags": ["Live2D"], "is_live2d_plus": false,
                                   "face_ids": ["1", "2"], "square_icon": "squareicon/biaoqiang_2",
                                   "shipyard_icon": "shipyardicon/biaoqiang_2", "q_icon": "qicon/biaoqiang_2"}},
  "files": {},
  "assets": []
}
```

Skin-level metadata (skin type badge, `featureTags` such as `Live2D` / `Live2D+` / `spine`,
face diff IDs, icon paths) lives in `source_metadata.costume`; ship-level metadata in
`source_metadata.character`. `source_metadata` is the crawler's full catalog entry, so it is
the authoritative place for anything not lifted into a first-class field.

### `files` shortcuts per model type

`files` maps well-known roles to asset objects (`null` / `[]` when absent):

- `live2d`: `model3`, `moc3`, `textures[]`, `physics`, `pose`, `display_info`,
  `expressions[]`, `motions[]`, `audio[]`. A motion's subtitle line (when the model carries
  one) is in the motion asset's `contexts[0].motion_text`, mirrored on its audio asset —
  there is no separate text file.
- `spine`: `parts`, `skel`, `skeletons[]`, `atlas`, `atlases[]`, `textures[]`
  (multi-part models list every part's skeleton/atlas/textures)
- `painting`: `image`, `faces[]`, `square_icon`, `shipyard_icon`, `voices[]`. There is no
  q-icon: the index advertises `qicon/<key>` for every skin but the CDN hosts none of them,
  so `source_metadata.costume.q_icon` is carried through as source data only.

### Asset object

```json
{
  "kind": "voice.audio",
  "path": "assets/painting/biaoqiang_2/cue/cv-1/detail.ogg",
  "url": "/static/azurlane/biaoqiang%20-%20Javelin/assets/painting/biaoqiang_2/cue/cv-1/detail.ogg?v=<sha256>",
  "source_url": "https://static.l2d.su/azurlane/cue/cv-1/detail.ogg",
  "content_type": "audio/ogg", "size": 123456, "sha256": "…",
  "status": "downloaded", "available": true,
  "model_id": "azurlane:painting:biaoqiang:biaoqiang_2",
  "field": "voice",
  "contexts": [{}]
}
```

`url` is the ready-to-use static URL (immutable-cacheable thanks to the `?v=<sha256>` bust).
Use `available` before rendering; `status` may be `pending` / `failed` for assets the crawler
has not (yet) fetched.

Asset kinds: `live2d.model3`, `live2d.moc3`, `live2d.texture`, `live2d.physics`,
`live2d.pose`, `live2d.display-info`, `live2d.expression`, `live2d.motion`, `live2d.audio`,
`spine.parts`, `spine.skel`, `spine.atlas`, `spine.texture`,
`painting.image`, `painting.face`, `icon.square`, `icon.shipyard`, `voice.audio`.

### Voice contexts

Each `voice.audio` asset's `contexts[0]` carries the full voice line, so the frontend needs no
extra request to render a voice list with text:

```json
{
  "key": "battle", "voice_name": "旗舰开战", "resource_key": "warcry",
  "voice_path": "cue/cv-960007-battle/warcry",
  "text": "风暴啊，请赐予我力量。",
  "face_id": "5", "l2d_action": "battle", "spine_action": "attack",
  "is_extra": false
}
```

`face_id` selects the painting face diff to show while the line plays; `l2d_action` /
`spine_action` name the model animation to trigger (mirroring l2d.su behavior). `is_extra`
marks lines from the skin's `extraWords`. Painting/face/icon assets carry
`painting_field` (`image` / `face` / `icon`) and `face_id` / `icon_path` in their contexts.

## Static files

Character directory layout on the collection volume (`/static/azurlane/{directory_name}/…`):

```
{character_key} - {display name}/
  manifest.json     # full character + models + assets (same data the API serves)
  character.json    # summary only
  detail.json       # raw l2d.su ship detail (skills, stats, acquisition, words, voice actors)
  assets/
    live2d/{model_key}/…             # model3.json tree as referenced by the model3
    spine/{model_key}/…              # parts manifest, skel/atlas/textures per part
    painting/{painting_key}/
      {painting_key}.webp            # the full painting
      paintingface/{painting_key}/{face_id}.webp
      squareicon/{key}.webp  shipyardicon/{key}.webp  qicon/{key}.webp
      cue/…/{line}.ogg               # voice files, mirroring the CDN path
```

The `?v=` cache-bust query only exists on API-provided URLs; raw nginx paths work without it.
Global source artifacts live in `/static/azurlane/_source/` (`l2d-su-snapshot.json` with
`filters` and `skin_update_history`, `nagami-snapshot.json`, `health-report.json`).

## Ship detail payload (l2d.su schema passthrough)

`ship-detail` / `detail.json` is the unmodified CN-region l2d.su payload:
`{version, locale, generatedAt, sourceRoot, ship}` where `ship` includes the index fields plus
`skills[]` (name/description), `stages[]` (per-breakout stats), `propertyHexagon`,
`acquisition`, `className`, `starRange`, and per skin `description`, `words[]`, `extraWords[]`,
`voiceActors[]`, `l2dAnimations[]`, `forms[]` / `isDualForm`, `model` (authoritative model
path). Treat unknown fields as forward-compatible extras — the crawler stores whatever l2d.su
returns.

## Freshness semantics

- The index (ships, skins, paintings, icons) refreshes every crawl run.
- Ship details refresh only when a ship's index fingerprint changes; on a fresh database the
  backfill of ~880 ships completes in one run, paced by
  `web.azurlane.origin_request_interval_seconds`. Until a ship's detail arrives, its
  `ship-detail` endpoint is 404, its voice assets are absent, and
  `source_metadata.class_name` is missing; all three appear automatically once it does.
  Everything else — paintings, faces, icons, models — is index-driven and available from the
  first run.
- A model's assets are immutable once `availability.archive_state` is `complete`; new skins
  arrive as new model entries.
