-- The consolidated fav schema: every table, column, constraint and index the Python
-- baseline creates, in its final shape, in one script (rust/docs/PLAN.md, WP1).
--
-- Python creates each source's tables the first time that source runs, with ALTER TABLE
-- statements for columns added later. Here every column sits in its CREATE TABLE, in the
-- position Python's ALTER gives it on a fresh database. rust/parity/schema.txt is what the
-- Python code creates on an empty database; fav-db's schema test checks that this script
-- creates exactly that.
--
-- Every statement is idempotent: the script runs at every start, also against a database
-- the Python code created, where a source that has not run since a column was added may
-- still lack it. So the script keeps Python's three blocks (AGENTS.md, "Database"):
-- tables, then every column added later (a no-op on a fresh database), then every index,
-- which may name such a column. Within the first block foreign keys need their targets
-- first.

-- ============================================================================
-- Tables
-- ============================================================================

-- src/core/settings.py (CREATE_APP_SETTINGS_TABLE_SQL)
CREATE TABLE IF NOT EXISTS app_settings (
    section    TEXT PRIMARY KEY,
    value      JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- src/api/live2d_overrides.py
CREATE TABLE IF NOT EXISTS live2d_view_overrides (
    source TEXT NOT NULL,
    content_id INTEGER NOT NULL,
    model_id TEXT NOT NULL,
    profile TEXT NOT NULL,
    position JSONB NOT NULL,
    scale DOUBLE PRECISION NOT NULL,
    background_position JSONB,
    background_scale DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (source, content_id, model_id, profile),
    CONSTRAINT live2d_view_overrides_source_check CHECK (source IN ('bd2', 'nikke')),
    CONSTRAINT live2d_view_overrides_profile_check CHECK (profile ~ '^[A-Za-z0-9_-]{1,64}$'),
    CONSTRAINT live2d_view_overrides_position_check CHECK (jsonb_typeof(position) = 'object'),
    CONSTRAINT live2d_view_overrides_background_position_check CHECK (
        background_position IS NULL OR jsonb_typeof(background_position) = 'object'
    ),
    CONSTRAINT live2d_view_overrides_scale_check CHECK (scale > 0),
    CONSTRAINT live2d_view_overrides_background_scale_check CHECK (background_scale IS NULL OR background_scale > 0)
);

-- src/tool/control_queue.py
CREATE TABLE IF NOT EXISTS control_requests (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL,
    requested_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TIMESTAMPTZ NULL,
    finished_at TIMESTAMPTZ NULL,
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT ''
);

-- src/tool/notifications.py
CREATE TABLE IF NOT EXISTS notifications (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    header TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    link_url TEXT NOT NULL DEFAULT '',
    image_url TEXT NOT NULL DEFAULT '',
    payload TEXT NOT NULL DEFAULT '{}',
    dedupe_key TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'unread',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    read_at TIMESTAMPTZ NULL,
    markdown TEXT NOT NULL DEFAULT '',
    disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE,
    disable_notification BOOLEAN NOT NULL DEFAULT TRUE,
    pin BOOLEAN NOT NULL DEFAULT FALSE,
    webhook_action TEXT NOT NULL DEFAULT 'send',
    occurrence_count INTEGER NOT NULL DEFAULT 1,
    event_version INTEGER NOT NULL DEFAULT 1,
    delivery_status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT 'item',
    telegram_message_id BIGINT NULL,
    telegram_pinned_message_id BIGINT NULL
);

-- src/tool/site_sessions.py (`extra` was added by an ALTER; folded in as the last column)
CREATE TABLE IF NOT EXISTS site_sessions (
    site TEXT PRIMARY KEY,
    cookies TEXT NOT NULL DEFAULT '[]',
    account TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    expires_at TIMESTAMPTZ NULL,
    signed_in_at TIMESTAMPTZ NULL,
    checked_at TIMESTAMPTZ NULL,
    extra TEXT NOT NULL DEFAULT '{}'
);

-- src/tool/telegram_queue.py
CREATE TABLE IF NOT EXISTS telegram_media_queue (
    account_name TEXT NOT NULL,
    channel_id BIGINT NOT NULL,
    message_id BIGINT NOT NULL,
    grouped_id BIGINT NULL,
    media_type TEXT NOT NULL,
    title TEXT NOT NULL,
    source TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending',
    attempt_count INTEGER NOT NULL DEFAULT 0,
    available_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    owner_token TEXT NULL,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ NULL,
    PRIMARY KEY (account_name, channel_id, message_id)
);

-- src/tool/hanime1_series.py
CREATE TABLE IF NOT EXISTS hanime1_series (
    canonical_video_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    added_from_video_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_scanned_at TIMESTAMPTZ,
    last_scan_error TEXT NOT NULL DEFAULT '',

    CHECK (canonical_video_id ~ '^[1-9][0-9]*$'),
    CHECK (added_from_video_id ~ '^[1-9][0-9]*$'),
    CHECK (btrim(title) <> '')
);

CREATE TABLE IF NOT EXISTS hanime1_series_video (
    video_id TEXT PRIMARY KEY,
    canonical_video_id TEXT NOT NULL REFERENCES hanime1_series(canonical_video_id) ON DELETE CASCADE,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CHECK (video_id ~ '^[1-9][0-9]*$')
);

CREATE TABLE IF NOT EXISTS hanime1_ranking_scan_state (
    id SMALLINT PRIMARY KEY DEFAULT 1,
    quota_debt DOUBLE PRECISION NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CHECK (id = 1)
);

-- src/tool/hanime1_author.py
CREATE TABLE IF NOT EXISTS hanime1_author (
    author_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    video_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_scanned_at TIMESTAMPTZ,
    last_scan_error TEXT NOT NULL DEFAULT '',

    CHECK (author_id ~ '^[1-9][0-9]*$'),
    CHECK (btrim(name) <> '')
);

-- src/web/bilibili.py
CREATE TABLE IF NOT EXISTS bilibili (
    bvid TEXT PRIMARY KEY,
    fav_id BIGINT NOT NULL,
    title TEXT NOT NULL,
    upper TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- src/web/telegram.py (_TELEGRAM_SCHEMA_SQL)
CREATE TABLE IF NOT EXISTS telegram (
    account_name TEXT NOT NULL,
    message_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    title TEXT NOT NULL,
    channel_name TEXT NOT NULL,
    media_type TEXT NOT NULL DEFAULT 'video',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, channel_id, message_id)
);

CREATE TABLE IF NOT EXISTS telegram_channel_state (
    account_name TEXT NOT NULL,
    channel_id BIGINT NOT NULL,
    last_scanned_message_id BIGINT NOT NULL DEFAULT 0,
    last_download_at TIMESTAMPTZ NULL,
    cooldown_until TIMESTAMPTZ NULL,
    last_error TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, channel_id)
);

CREATE TABLE IF NOT EXISTS telegram_channel_media_backfill (
    account_name TEXT NOT NULL,
    channel_id BIGINT NOT NULL,
    media_type TEXT NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (account_name, channel_id, media_type)
);

-- src/web/nikke.py (_CREATE_SCHEMA_SQL)
CREATE TABLE IF NOT EXISTS nikke_pages (
    content_id BIGINT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    source_url TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    raw_list_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_detail_json JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS nikke_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS nikke_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS nikke_page_assets (
    content_id BIGINT NOT NULL REFERENCES nikke_pages(content_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES nikke_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_id, url, kind, context_hash)
);

-- src/web/bd2.py (_CREATE_SCHEMA_SQL)
CREATE TABLE IF NOT EXISTS bd2_pages (
    content_id BIGINT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    source_url TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    rarity_group_id BIGINT NOT NULL DEFAULT 0,
    rarity_group_name TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    raw_tree_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    raw_detail_json JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS bd2_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    next_retry_at TIMESTAMPTZ,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS bd2_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS bd2_page_assets (
    content_id BIGINT NOT NULL REFERENCES bd2_pages(content_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES bd2_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (content_id, url, kind, context_hash)
);

-- src/web/azurlane.py (_CREATE_SCHEMA_SQL)
CREATE TABLE IF NOT EXISTS azurlane_characters (
    character_key TEXT PRIMARY KEY,
    source_id BIGINT,
    name_zh TEXT NOT NULL DEFAULT '',
    name_en TEXT NOT NULL DEFAULT '',
    manifest_path TEXT NOT NULL DEFAULT '',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    source_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS azurlane_models (
    model_id TEXT PRIMARY KEY,
    model_type TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT '',
    character_key TEXT NOT NULL REFERENCES azurlane_characters(character_key) ON DELETE CASCADE,
    costume_key TEXT NOT NULL DEFAULT '',
    costume_id BIGINT,
    costume_name_zh TEXT NOT NULL DEFAULT '',
    costume_name_en TEXT NOT NULL DEFAULT '',
    primary_url TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    display_info_url TEXT NOT NULL DEFAULT '',
    availability_state TEXT NOT NULL DEFAULT 'unchecked',
    active BOOLEAN NOT NULL DEFAULT TRUE,
    source_metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    last_attempt_at TIMESTAMPTZ,
    next_retry_at TIMESTAMPTZ,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    fetched_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS azurlane_assets (
    url TEXT PRIMARY KEY,
    normalized_url TEXT NOT NULL DEFAULT '',
    downloaded_url TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT '',
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    content_type TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'pending',
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    last_attempt_at TIMESTAMPTZ,
    next_retry_at TIMESTAMPTZ,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS azurlane_blobs (
    sha256 TEXT NOT NULL,
    size BIGINT NOT NULL,
    content_type TEXT NOT NULL DEFAULT '',
    blob_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ,
    PRIMARY KEY (sha256, size)
);

CREATE TABLE IF NOT EXISTS azurlane_ship_details (
    ship_group_id BIGINT PRIMARY KEY,
    region TEXT NOT NULL DEFAULT '',
    fingerprint TEXT NOT NULL DEFAULT '',
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS azurlane_model_assets (
    model_id TEXT NOT NULL REFERENCES azurlane_models(model_id) ON DELETE CASCADE,
    url TEXT NOT NULL REFERENCES azurlane_assets(url) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    local_path TEXT NOT NULL DEFAULT '',
    original_filename TEXT NOT NULL DEFAULT '',
    fallback_url TEXT NOT NULL DEFAULT '',
    context_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (model_id, url, kind, context_hash)
);

-- src/web/hanime1.py
CREATE TABLE IF NOT EXISTS hanime1 (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    uploader TEXT,
    release_date TEXT,
    plot TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- src/web/jandan.py
CREATE TABLE IF NOT EXISTS jandan (
    fav_type INTEGER NOT NULL,
    pic_id INTEGER NOT NULL,
    content_index INTEGER NOT NULL,
    content_md5 TEXT,
    content_url TEXT NOT NULL,
    author TEXT NOT NULL DEFAULT '',
    post_date TEXT NOT NULL DEFAULT '',
    local_path TEXT NOT NULL DEFAULT '',
    downloaded INTEGER NOT NULL DEFAULT 0,
    unavailable INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (fav_type, pic_id, content_index)
);

-- src/web/kemono.py
CREATE TABLE IF NOT EXISTS kemono (
    id TEXT NOT NULL,
    service TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT NOT NULL,
    creator TEXT,
    published_at TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id, service, user_id)
);

CREATE TABLE IF NOT EXISTS kemono_pending_originals (
    service TEXT NOT NULL,
    user_id TEXT NOT NULL,
    post_id TEXT NOT NULL,
    path TEXT NOT NULL,
    dst TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_check_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (service, user_id, post_id, path)
);

-- src/web/twitter.py
CREATE TABLE IF NOT EXISTS twitter (
    tweet_id BIGINT NOT NULL,
    num INTEGER NOT NULL,
    author TEXT NOT NULL DEFAULT '',
    author_nick TEXT NOT NULL DEFAULT '',
    author_id TEXT NOT NULL DEFAULT '',
    content TEXT NOT NULL DEFAULT '',
    tweet_date TEXT NOT NULL DEFAULT '',
    media_type TEXT NOT NULL DEFAULT '',
    local_path TEXT NOT NULL DEFAULT '',
    metadata JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tweet_id, num)
);

CREATE TABLE IF NOT EXISTS twitter_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- src/web/pixiv.py
CREATE TABLE IF NOT EXISTS pixiv (
    illust_id BIGINT NOT NULL,
    num INTEGER NOT NULL,
    illust_type INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    author_id TEXT NOT NULL DEFAULT '',
    page_count INTEGER NOT NULL DEFAULT 1,
    source_url TEXT NOT NULL DEFAULT '',
    local_path TEXT NOT NULL DEFAULT '',
    downloaded INTEGER NOT NULL DEFAULT 0,
    unavailable INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (illust_id, num)
);

-- src/web/rednote.py
CREATE TABLE IF NOT EXISTS rednote (
    note_id TEXT NOT NULL,
    media_index INTEGER NOT NULL,
    media_type TEXT NOT NULL DEFAULT '',
    media_url TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    author_id TEXT NOT NULL DEFAULT '',
    note_type TEXT NOT NULL DEFAULT '',
    published_at TEXT NOT NULL DEFAULT '',
    xsec_token TEXT NOT NULL DEFAULT '',
    local_path TEXT NOT NULL DEFAULT '',
    downloaded INTEGER NOT NULL DEFAULT 0,
    unavailable INTEGER NOT NULL DEFAULT 0,
    failed_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (note_id, media_index)
);

CREATE TABLE IF NOT EXISTS rednote_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rednote_missing (
    note_id TEXT PRIMARY KEY,
    runs INTEGER NOT NULL DEFAULT 0,
    last_run TEXT NOT NULL DEFAULT '',
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- src/web/wechat.py
CREATE TABLE IF NOT EXISTS wechat_inbox (
    msg_id BIGINT NOT NULL,
    create_time BIGINT NOT NULL,
    talker TEXT NOT NULL DEFAULT '',
    sender TEXT NOT NULL DEFAULT '',
    sent_at TEXT NOT NULL DEFAULT '',
    md5 TEXT NOT NULL DEFAULT '',
    size BIGINT,
    local_path TEXT NOT NULL DEFAULT '',
    metadata JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (msg_id, create_time)
);

CREATE TABLE IF NOT EXISTS wechat_article (
    msg_id BIGINT NOT NULL,
    create_time BIGINT NOT NULL,
    talker TEXT NOT NULL DEFAULT '',
    sender TEXT NOT NULL DEFAULT '',
    sent_at TEXT NOT NULL DEFAULT '',
    url TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    account TEXT NOT NULL DEFAULT '',
    published_at TEXT NOT NULL DEFAULT '',
    image_count INTEGER NOT NULL DEFAULT 0,
    local_path TEXT NOT NULL DEFAULT '',
    metadata JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (msg_id, create_time)
);

CREATE TABLE IF NOT EXISTS wechat_channel_video (
    msg_id BIGINT NOT NULL,
    create_time BIGINT NOT NULL,
    talker TEXT NOT NULL DEFAULT '',
    sender TEXT NOT NULL DEFAULT '',
    sent_at TEXT NOT NULL DEFAULT '',
    object_id TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    duration INTEGER NOT NULL DEFAULT 0,
    size BIGINT,
    local_path TEXT NOT NULL DEFAULT '',
    metadata JSONB,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (msg_id, create_time)
);

CREATE TABLE IF NOT EXISTS wechat_events (
    msg_id BIGINT NOT NULL,
    create_time BIGINT NOT NULL,
    work TEXT NOT NULL,
    seq BIGINT NOT NULL DEFAULT 0,
    message JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (msg_id, create_time)
);

CREATE TABLE IF NOT EXISTS wechat_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ============================================================================
-- Columns added after a table was first created, for databases that predate them
-- ============================================================================

ALTER TABLE notifications ADD COLUMN IF NOT EXISTS dedupe_key TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS header TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS markdown TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_web_page_preview BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS disable_notification BOOLEAN NOT NULL DEFAULT TRUE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS pin BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS webhook_action TEXT NOT NULL DEFAULT 'send';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS occurrence_count INTEGER NOT NULL DEFAULT 1;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS event_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivery_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS attempt_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS delivered_at TIMESTAMPTZ NULL;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT 'item';
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS telegram_message_id BIGINT NULL;
ALTER TABLE notifications ADD COLUMN IF NOT EXISTS telegram_pinned_message_id BIGINT NULL;
ALTER TABLE site_sessions ADD COLUMN IF NOT EXISTS extra TEXT NOT NULL DEFAULT '{}';
ALTER TABLE telegram ADD COLUMN IF NOT EXISTS media_type TEXT NOT NULL DEFAULT 'video';
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS last_download_at TIMESTAMPTZ NULL;
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS cooldown_until TIMESTAMPTZ NULL;
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';
ALTER TABLE telegram_channel_state ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE bd2_assets ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;
ALTER TABLE azurlane_models ADD COLUMN IF NOT EXISTS failed_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE azurlane_models ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_models ADD COLUMN IF NOT EXISTS last_attempt_at TIMESTAMPTZ;
ALTER TABLE azurlane_models ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS downloaded_url TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS fallback_url TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT '';
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS last_attempt_at TIMESTAMPTZ;
ALTER TABLE azurlane_assets ADD COLUMN IF NOT EXISTS next_retry_at TIMESTAMPTZ;
ALTER TABLE azurlane_model_assets ADD COLUMN IF NOT EXISTS fallback_url TEXT NOT NULL DEFAULT '';
ALTER TABLE hanime1 ADD COLUMN IF NOT EXISTS release_date TEXT;
ALTER TABLE hanime1 ADD COLUMN IF NOT EXISTS plot TEXT;
ALTER TABLE hanime1 ADD COLUMN IF NOT EXISTS title TEXT NOT NULL DEFAULT '';
ALTER TABLE hanime1 ADD COLUMN IF NOT EXISTS uploader TEXT;
ALTER TABLE jandan ADD COLUMN IF NOT EXISTS unavailable INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jandan ADD COLUMN IF NOT EXISTS failed_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jandan ADD COLUMN IF NOT EXISTS last_error TEXT NOT NULL DEFAULT '';

-- src/web/bilibili.py: databases from before `bvid` was the primary key get the unique
-- constraint its ON CONFLICT needs (`_BILIBILI_BVID_ARBITER_SQL`).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_index AS idx
        JOIN pg_attribute AS attr
          ON attr.attrelid = idx.indrelid
         AND attr.attnum = idx.indkey[0]
        WHERE idx.indrelid = 'bilibili'::regclass
          AND idx.indisunique
          AND idx.indimmediate
          AND idx.indisvalid
          AND idx.indisready
          AND idx.indnkeyatts = 1
          AND idx.indpred IS NULL
          AND idx.indexprs IS NULL
          AND attr.attname = 'bvid'
    ) THEN
        ALTER TABLE bilibili ADD CONSTRAINT bilibili_bvid_unique UNIQUE (bvid);
    END IF;
END
$$;

-- ============================================================================
-- Indexes
-- ============================================================================

-- src/tool/notifications.py
CREATE UNIQUE INDEX IF NOT EXISTS notifications_dedupe_key_unique
ON notifications (dedupe_key)
WHERE dedupe_key <> '';

-- src/tool/telegram_queue.py
CREATE INDEX IF NOT EXISTS telegram_media_queue_claim_idx
ON telegram_media_queue (account_name, status, priority DESC, available_at, created_at)
WHERE status = 'pending';

-- src/tool/hanime1_series.py
CREATE INDEX IF NOT EXISTS idx_hanime1_series_video_series
ON hanime1_series_video (canonical_video_id);

-- src/web/nikke.py (_CREATE_SCHEMA_SQL)
CREATE INDEX IF NOT EXISTS nikke_assets_sha256_size_idx ON nikke_assets (sha256, size);
CREATE INDEX IF NOT EXISTS nikke_page_assets_content_id_idx ON nikke_page_assets (content_id);

-- src/web/bd2.py (_CREATE_SCHEMA_SQL)
CREATE INDEX IF NOT EXISTS bd2_assets_sha256_size_idx ON bd2_assets (sha256, size);
CREATE INDEX IF NOT EXISTS bd2_page_assets_content_id_idx ON bd2_page_assets (content_id);
CREATE INDEX IF NOT EXISTS bd2_pages_rarity_group_id_idx ON bd2_pages (rarity_group_id);

-- src/web/azurlane.py (_CREATE_SCHEMA_SQL)
CREATE INDEX IF NOT EXISTS azurlane_models_character_key_idx ON azurlane_models (character_key);
CREATE INDEX IF NOT EXISTS azurlane_models_retry_idx ON azurlane_models (next_retry_at);
CREATE INDEX IF NOT EXISTS azurlane_assets_sha256_size_idx ON azurlane_assets (sha256, size);
CREATE INDEX IF NOT EXISTS azurlane_assets_status_retry_idx ON azurlane_assets (status, next_retry_at);
CREATE INDEX IF NOT EXISTS azurlane_model_assets_model_id_idx ON azurlane_model_assets (model_id);
