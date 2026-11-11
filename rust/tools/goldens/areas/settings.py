"""Settings defaults as `save_section` writes them (WP1), and the section registry.

`app_settings.value` is JSONB, so PostgreSQL normalises the text it is given: what matters
is the value, not these bytes. The text is kept so a Rust writer can be diffed against it
before the database sees it.
"""

from __future__ import annotations

import json
from typing import Any

from src.core import settings


def _stored_defaults() -> list[dict[str, Any]]:
    # `save_section` writes json.dumps(json.loads(model.model_dump_json()), ensure_ascii=False).
    cases = []
    for section, model in settings.SECTION_MODELS.items():
        serialized = json.loads(model().model_dump_json())
        cases.append({'section': section, 'stored': json.dumps(serialized, ensure_ascii=False)})
    return cases


def _registry() -> list[dict[str, Any]]:
    return [
        {
            'section': section,
            'model': model.__name__,
            'sensitive_fields': list(settings.SENSITIVE_FIELDS.get(section, ())),
        }
        for section, model in settings.SECTION_MODELS.items()
    ]


def _full_tree() -> list[dict[str, Any]]:
    return [{'settings': json.loads(settings.Settings().model_dump_json())}]


GOLDENS = {
    'settings/stored_defaults.json': _stored_defaults,
    'settings/sections.json': _registry,
    'settings/default_tree.json': _full_tree,
}
