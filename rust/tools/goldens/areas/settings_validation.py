"""How the settings models validate input (WP1): coerced values, or pydantic's error list.

The settings API stores what `model_validate` makes of a form payload and answers a bad one
with `json.loads(exc.json())` as the error details, so both the accepted value and every
error entry (type, loc, msg, input, ctx, url) are observable. Each case gives the model, the
input as JSON text (what `json.loads` hands the API), and either the dumped model
(`json.loads(model.model_dump_json())`, what `save_section` stores) or the error list.
Valid cases also carry what `validate_runnable()` reports, where the model has it.
"""

from __future__ import annotations

import json
from typing import Any

from pydantic import BaseModel, ValidationError
from src.core import settings

MODELS: dict[str, type[BaseModel]] = {
    'ScheduleJob': settings.ScheduleJob,
    'BilibiliFavorite': settings.BilibiliFavorite,
    'BilibiliAccount': settings.BilibiliAccount,
    'TelegramChannel': settings.TelegramChannel,
    'TelegramAccount': settings.TelegramAccount,
    'Hanime1RankingDeepScan': settings.Hanime1RankingDeepScan,
    'Hanime1Ranking': settings.Hanime1Ranking,
    'KemonoCreator': settings.KemonoCreator,
    'Settings': settings.Settings,
    **dict(settings.SECTION_MODELS.items()),
}

_CHANNEL = '{"id": 1, "path": "./collection/telegram/main"}'
_ACCOUNT = '{"name": "main", "channels": [' + _CHANNEL + '], "api_id": 123, "api_hash": "hash-1", "session_path": "./data/main"}'

# (model, input JSON text). Grouped by what they exercise; every model also gets `{}`.
CASES: list[tuple[str, str]] = [
    # Defaults and the shape of a missing or non-object input.
    *[(name, '{}') for name in MODELS if name not in {'ScheduleJob', 'BilibiliFavorite', 'BilibiliAccount', 'TelegramChannel'}],
    ('TelegramAccount', '{}'),
    ('KemonoCreator', '{}'),
    ('ScheduleJob', '{}'),
    ('BilibiliFavorite', '{}'),
    ('BilibiliAccount', '{}'),
    ('TelegramChannel', '{}'),
    ('web.bd2', 'null'),
    ('web.bd2', '[]'),
    ('web.bd2', '"text"'),
    ('web.bd2', '1'),
    # Unknown keys are ignored, also ones left behind by removed features.
    ('web.wechat', '{"token": "t", "nga_cookiecloud": "main", "tikhub_token": "k"}'),
    # ScheduleJob: the cron field count and the lax coercion of bool and str.
    ('ScheduleJob', '{"cron": "*/5 * * * *"}'),
    ('ScheduleJob', '{"cron": "*/5 * * *"}'),
    ('ScheduleJob', '{"cron": "*/5 * * * * *"}'),
    ('ScheduleJob', '{"cron": "  */5\\t*\\n*  * \\u3000 *  "}'),
    ('ScheduleJob', '{"cron": "*/5\\u00a0* * * *"}'),
    ('ScheduleJob', '{"cron": 5}'),
    ('ScheduleJob', '{"cron": null}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": "yes", "notify": "off"}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": 1, "notify": 0}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": 1.0, "notify": 0.0}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": 2, "notify": 0.5}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": "TRUE", "notify": "F"}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": " true", "notify": null}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": "Y", "notify": "No"}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": "t", "notify": "0"}'),
    ('ScheduleJob', '{"cron": "* * * * *", "enabled": [], "notify": {}}'),
    ('ScheduleJob', '{"cron": 1.5, "enabled": "maybe", "notify": "1"}'),
    # Bilibili.
    ('web.bilibili', '{"proxy": "  http://hk:1  ", "accounts": [{"name": " main "}]}'),
    ('web.bilibili', '{"accounts": [{"name": "main", "favorites": [{"fav_id": 1, "path": "a"}], "toview_enabled": true}]}'),
    ('web.bilibili', '{"accounts": [{"name": "main"}, {"name": "MAIN"}]}'),
    ('web.bilibili', '{"accounts": [{"name": "main"}, {"name": "alt", "favorites": [{"fav_id": 0, "path": "a"}]}, {"name": "Alt"}]}'),
    ('web.bilibili', '{"accounts": [{"name": ""}, {"name": "a b"}, {"name": "ok-1_x"}]}'),
    ('web.bilibili', '{"accounts": [{"name": "\\u00e9"}]}'),
    ('web.bilibili', '{"accounts": "main"}'),
    ('web.bilibili', '{"accounts": [1, null]}'),
    ('web.bilibili', '{"proxy": 1, "cron": "x", "enabled": "x"}'),
    ('BilibiliAccount', '{"name": "main", "favorites": [{"fav_id": 7, "path": "a"}, {"fav_id": 7, "path": "b"}]}'),
    ('BilibiliAccount', '{"name": "main", "favorites": [{"fav_id": "7", "path": "./a/b/"}], "toview_path": "//x//y"}'),
    ('BilibiliAccount', '{"name": "main", "favorites": [{"fav_id": -1}]}'),
    ('BilibiliAccount', '{"name": "main", "toview_enabled": true}'),
    ('BilibiliAccount', '{"name": "main"}'),
    ('BilibiliFavorite', '{"fav_id": 1.0, "path": ""}'),
    ('BilibiliFavorite', '{"fav_id": 1.5, "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": " 12 ", "path": "."}'),
    ('BilibiliFavorite', '{"fav_id": "1_000", "path": "a/./b/../c"}'),
    ('BilibiliFavorite', '{"fav_id": "+5", "path": "///abs"}'),
    ('BilibiliFavorite', '{"fav_id": "1.0", "path": "/"}'),
    ('BilibiliFavorite', '{"fav_id": "0x10", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": true, "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": 1e20, "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": null, "path": 1}'),
    ('BilibiliFavorite', '{"fav_id": "", "path": null}'),
    ('BilibiliFavorite', '{"fav_id": "1.5", "path": []}'),
    ('BilibiliFavorite', '{"fav_id": "-0", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": "1.00", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": "12abc", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": "1__0", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": "_1", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": "\\u0661\\u0662", "path": "a"}'),
    ('BilibiliFavorite', '{"fav_id": 9223372036854775807, "path": "a"}'),
    # Telegram: channel ids, media routes, limits and delays.
    ('TelegramChannel', '{"id": 1, "path": "./collection/telegram/main"}'),
    ('TelegramChannel', '{"id": -1002522897097, "path": "a"}'),
    ('TelegramChannel', '{"id": -1001, "path": "a"}'),
    ('TelegramChannel', '{"id": -1000, "path": "a"}'),
    ('TelegramChannel', '{"id": -100, "path": "a"}'),
    ('TelegramChannel', '{"id": -5, "path": "a"}'),
    ('TelegramChannel', '{"id": 0, "path": "a"}'),
    ('TelegramChannel', '{"id": "-1001", "path": "a"}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "media_types": ["video", "image", "image"]}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "media_types": []}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "media_types": ["photo"]}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "media_types": ["image", 1, null]}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "media_types": "video"}'),
    ('TelegramChannel', '{"id": 1, "path": "a", "report_unsupported": "on"}'),
    ('TelegramAccount', _ACCOUNT),
    ('TelegramAccount', '{"name": ".."}'),
    ('TelegramAccount', '{"name": "main"}'),
    ('TelegramAccount', '{"name": "main", "api_id": 1, "api_hash": "   ", "channels": []}'),
    (
        'TelegramAccount',
        '{"name": "main", "api_id": 123, "api_hash": "hash", "channels": ['
        '{"id": 3942401424, "path": "./collection/image", "media_types": ["image"]},'
        '{"id": 3942401424, "path": "./collection/video", "media_types": ["video"]}]}',
    ),
    (
        'TelegramAccount',
        '{"name": "main", "channels": [{"id": 1, "path": "one", "media_types": ["image"]}, '
        '{"id": 1, "path": "two", "media_types": ["image"]}]}',
    ),
    ('TelegramAccount', '{"name": "main", "channels": [{"id": -1002522897097, "path": "one"}, {"id": 2522897097, "path": "two"}]}'),
    ('TelegramAccount', '{"name": "main", "channels": [{"id": 0, "path": "one"}, {"id": 1, "path": "one"}, {"id": 1, "path": "two"}]}'),
    ('web.telegram', '{"accounts": []}'),
    ('web.telegram', '{"accounts": [' + _ACCOUNT + ']}'),
    ('web.telegram', '{"accounts": [' + _ACCOUNT + ', {"name": "MAIN"}]}'),
    ('web.telegram', '{"accounts": [{"name": "main"}, {"name": "alt", "api_id": 5, "api_hash": "h"}]}'),
    ('web.telegram', '{"scan_limit": 0, "download_limit_per_channel": 0}'),
    ('web.telegram', '{"download_delay_seconds": -1, "channel_cooldown_seconds": "inf", "history_wait_seconds": "nan"}'),
    ('web.telegram', '{"download_delay_seconds": "Infinity", "channel_cooldown_seconds": -0.0, "history_wait_seconds": 0}'),
    ('web.telegram', '{"download_delay_seconds": NaN}'),
    ('web.telegram', '{"download_delay_seconds": "1e3", "channel_cooldown_seconds": " 2 ", "history_wait_seconds": "1_0"}'),
    ('web.telegram', '{"download_delay_seconds": true, "channel_cooldown_seconds": 5, "history_wait_seconds": "x"}'),
    ('web.telegram', '{"download_delay_seconds": null, "flood_sleep_threshold_seconds": -1}'),
    ('web.telegram', '{"flood_sleep_threshold_seconds": 0, "scan_limit": "3", "download_limit_per_channel": 2.0}'),
    # Games and other simple sources.
    ('web.nikke', '{"runtime_capture_enabled": true, "runtime_capture_timeout_seconds": 5, "path": "/data/nikke"}'),
    ('web.azurlane', '{"origin_proxy": "  http://p  ", "origin_connect_host": "  ", "origin_request_interval_seconds": 0}'),
    ('web.azurlane', '{"origin_request_interval_seconds": -0.5}'),
    ('web.azurlane', '{"origin_request_interval_seconds": "nan"}'),
    # Hanime1 and its ranking.
    ('Hanime1Ranking', '{"enabled": true, "periods": ["weekly", "weekly", "monthly"], "pages": 1}'),
    ('Hanime1Ranking', '{"periods": []}'),
    ('Hanime1Ranking', '{"pages": 0}'),
    ('Hanime1Ranking', '{"periods": ["daily"], "pages": "x"}'),
    ('Hanime1Ranking', '{"deep_scan": {"quota": 0}}'),
    ('Hanime1Ranking', '{"deep_scan": null}'),
    ('Hanime1RankingDeepScan', '{"enabled": true, "quota": 0.5, "max_extra_pages": 3}'),
    ('Hanime1RankingDeepScan', '{"quota": 0.0}'),
    ('Hanime1RankingDeepScan', '{"quota": -1.0}'),
    ('Hanime1RankingDeepScan', '{"quota": Infinity}'),
    ('Hanime1RankingDeepScan', '{"quota": NaN}'),
    ('Hanime1RankingDeepScan', '{"quota": "inf"}'),
    ('Hanime1RankingDeepScan', '{"max_extra_pages": 0}'),
    ('Hanime1RankingDeepScan', '{"quota": 2, "max_extra_pages": "4"}'),
    ('web.hanime1', '{"user_lang": "zht", "host": "https://h.example/", "ranking": {"enabled": true}}'),
    ('web.hanime1', '{"user_lang": "en"}'),
    ('web.hanime1', '{"user_lang": null, "ranking": []}'),
    # Jandan.
    ('web.jandan', '{"user_id": 42, "fav_types": [6, 1, 6]}'),
    ('web.jandan', '{"fav_types": []}'),
    ('web.jandan', '{"fav_types": [3]}'),
    ('web.jandan', '{"fav_types": ["1", 2.0, true]}'),
    ('web.jandan', '{"user_id": -1}'),
    # Kemono.
    ('web.kemono', '{"base_url": "  https://k.example///  ", "file_base_url": "http://f", "thumbnail_base_url": "https://t/"}'),
    ('web.kemono', '{"base_url": "ftp://k", "file_base_url": "k", "thumbnail_base_url": ""}'),
    ('web.kemono', '{"sleep_request_seconds": -1}'),
    ('web.kemono', '{"creators": [{"service": "fanbox", "id": "1", "name": "n"}]}'),
    ('web.kemono', '{"creators": [{"service": "fanbox", "id": 1}]}'),
    ('KemonoCreator', '{"service": "s", "id": "i", "name": "n", "extra": 1}'),
    # Twitter, Pixiv, RedNote, WeChat.
    ('web.twitter', '{"username": "  @@me_1 ", "video_path": "  ", "proxy": " p ", "abort_after": 1}'),
    ('web.twitter', '{"username": "me", "video_path": "/videos"}'),
    ('web.twitter', '{"video_path": null}'),
    ('web.twitter', '{"video_path": ""}'),
    ('web.twitter', '{"video_path": 5}'),
    ('web.twitter', '{"username": "not valid"}'),
    ('web.twitter', '{"username": "@"}'),
    ('web.twitter', '{"username": "m\\u00e9"}'),
    ('web.twitter', '{"sleep_request_seconds": -0.1, "abort_after": 0}'),
    ('web.pixiv', '{"user_id": " 123 ", "proxy": " p "}'),
    ('web.pixiv', '{"user_id": "12a"}'),
    ('web.pixiv', '{"user_id": "\\u0661\\u0662"}'),
    ('web.pixiv', '{"user_id": "\\u00b2"}'),
    ('web.pixiv', '{"user_id": "\\u00bd"}'),
    ('web.pixiv', '{"user_id": "\\u4e00"}'),
    ('web.pixiv', '{"user_id": "\\uff11\\uff12"}'),
    ('web.pixiv', '{"sleep_request_seconds": -1}'),
    ('web.rednote', '{"proxy": "  http://res:1  ", "user_id": " u ", "video_path": "\\t"}'),
    ('web.rednote', '{"allow_direct_connection": true}'),
    ('web.rednote', '{"abort_after": 0, "max_pages_per_run": 0, "sleep_request_seconds": -1}'),
    ('web.rednote', '{"max_pages_per_run": "0"}'),
    ('web.wechat', '{"api_url": "  http://favinbox/  ", "token": "  t  "}'),
    ('web.wechat', '{"api_url": "", "token": ""}'),
    ('web.wechat', '{"api_url": "  ", "token": " "}'),
    # Notifications and shared services.
    ('notifications.telegram', '{"enabled": true, "bot_token": " 123:token ", "chat_id": " -100123 ", "message_thread_id": 42}'),
    ('notifications.telegram', '{"message_thread_id": 0}'),
    ('notifications.telegram', '{"message_thread_id": -3}'),
    ('notifications.telegram', '{"message_thread_id": null}'),
    ('notifications.telegram', '{"message_thread_id": "7"}'),
    ('notifications.telegram', '{"message_thread_id": "x"}'),
    ('notifications.telegram', '{"enabled": true}'),
    ('notifications.telegram', '{"bot_token": "b"}'),
    ('services.tikhub', '{"api_url": " https://api.tikhub.dev/ ", "token": " k "}'),
    ('services.tikhub', '{"api_url": 1, "token": null}'),
    # The whole tree, as build_settings assembles it from rows.
    ('Settings', '{"web": {"bd2": {"enabled": true}}, "services": {"tikhub": {"token": "k"}}}'),
    ('Settings', '{"web": {"bd2": {"cron": "x"}, "jandan": {"fav_types": []}}, "notifications": {"telegram": {"message_thread_id": 0}}}'),
    ('Settings', '{"web": null}'),
]


# Lax-mode coercion, one field per target type, none with a validator of its own: a float
# (`runtime_capture_timeout_seconds`), an int (`fav_num_limit`), a bool (`enabled`), a str
# (`host`) and a Path (`path`).
COERCION_FIELDS: list[tuple[str, str]] = [
    ('web.nikke', 'runtime_capture_timeout_seconds'),
    ('web.jandan', 'fav_num_limit'),
    ('web.bd2', 'enabled'),
    ('web.hanime1', 'host'),
    ('web.bd2', 'path'),
]
COERCION_INPUTS: list[str] = [
    'null',
    'true',
    'false',
    '0',
    '1',
    '-1',
    '2',
    '7',
    '0.0',
    '1.0',
    '-0.0',
    '1.5',
    '-2.5',
    '1e18',
    '9.3e18',
    '1e20',
    '9223372036854775807',
    '-9223372036854775808',
    'NaN',
    'Infinity',
    '-Infinity',
    '[]',
    '{}',
    '[1]',
    '""',
    '" "',
    '"0"',
    '"1"',
    '"2"',
    '"-1"',
    '"+1"',
    '"007"',
    '"1.0"',
    '"1.00"',
    '"1."',
    '"1.5"',
    '"-1.0"',
    '"1e3"',
    '"1E3"',
    '"1.5e3"',
    '".5"',
    '"5."',
    '"+.5"',
    '"-.5e-1"',
    '"1_000"',
    '"1__0"',
    '"_1"',
    '"1_"',
    '"1_0.5"',
    '"1.0_0"',
    '" 12 "',
    '"\\t12\\n"',
    '"\\u3000 12"',
    '"\\u00a012"',
    '"\\u001f12"',
    '"0x10"',
    '"0b1"',
    '"1e"',
    '"e3"',
    '"abc"',
    '"nan"',
    '"NaN"',
    '"-nan"',
    '"inf"',
    '"-inf"',
    '"+inf"',
    '"Infinity"',
    '"infinity"',
    '"INF"',
    '"infinit"',
    '"\\u0661\\u0662"',
    '"\\uff11"',
    '"true"',
    '"false"',
    '"True"',
    '"tRuE"',
    '"yes"',
    '"no"',
    '"on"',
    '"off"',
    '"y"',
    '"n"',
    '"t"',
    '"f"',
    '"1 "',
    '"00"',
    '"0.0"',
    '"9223372036854775808"',
    '"99999999999999999999"',
    '"./a/b/"',
    '"a//b"',
    '"."',
    '"./"',
    '"//x"',
    '"///x"',
    '"/a/./b/../c"',
    '"a/.."',
    '"~/x"',
    '"a\\\\b"',
]


def _runnable(model: BaseModel) -> list[str] | None:
    method = getattr(model, 'validate_runnable', None)
    return method() if callable(method) else None


def _validation() -> list[dict[str, Any]]:
    cases = []
    seen: set[tuple[str, str]] = set()
    coercion = [(model, f'{{"{field}": {raw}}}') for model, field in COERCION_FIELDS for raw in COERCION_INPUTS]
    for name, text in [*CASES, *coercion]:
        if (name, text) in seen:
            continue
        seen.add((name, text))
        case: dict[str, Any] = {'model': name, 'input': text}
        try:
            model = MODELS[name].model_validate(json.loads(text))
        except ValidationError as exc:
            case['errors'] = json.loads(exc.json())
        else:
            case['value'] = json.loads(model.model_dump_json())
            runnable = _runnable(model)
            if runnable is not None:
                case['runnable'] = runnable
        cases.append(case)
    return cases


# Rows as `app_settings` holds them, and what `build_settings` makes of them.
ROWS: list[str] = [
    '{}',
    '{"web.bd2": {"enabled": true}, "web.unknown": {"x": 1}, "cookiecloud": {"configs": []}}',
    '{"web.wechat": {"token": "t", "nga_cookiecloud": "main"}, "notifications.telegram": {"bot_token": "b", "chat_id": "c"}}',
    '{"services.tikhub": {"token": "k"}, "web.telegram": {"accounts": []}}',
    '{"web.jandan": {"fav_types": []}}',
    '{"web.bd2": null}',
]


def _build_settings() -> list[dict[str, Any]]:
    cases = []
    for text in ROWS:
        case: dict[str, Any] = {'rows': text}
        try:
            built = settings.build_settings(json.loads(text))
        except ValidationError as exc:
            case['errors'] = json.loads(exc.json())
        else:
            case['value'] = json.loads(built.model_dump_json())
        cases.append(case)
    return cases


GOLDENS = {
    'settings/validation.json': _validation,
    'settings/build_settings.json': _build_settings,
}
