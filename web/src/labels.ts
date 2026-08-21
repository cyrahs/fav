/**
 * Display names for the sources, keyed by settings section.
 *
 * The jobs list, the overview and the archive views each carry a `name` from the
 * backend, which is written for API consumers rather than for this Chinese UI.
 * Everything user-facing goes through here instead, so one source can never show
 * up under two different names depending on the page.
 */
const SECTION_LABELS: Record<string, string> = {
  'web.bilibili': 'Bilibili',
  'web.telegram': 'Telegram',
  'web.stellasora': 'Stella Sora',
  'web.nikke': 'Nikke',
  'web.bd2': 'BD2',
  'web.azurlane': 'Azur Lane',
  'web.hanime1': 'Hanime1',
  'web.jandan': '煎蛋',
  'web.kemono': 'Kemono',
  'web.twitter': 'X',
  'web.pixiv': 'Pixiv',
  'web.rednote': '小红书',
  'notifications.telegram': 'Telegram 通知',
};

/** `fallback` is the backend name, used for a section this build predates. */
export function sectionLabel(section: string, fallback = ''): string {
  return SECTION_LABELS[section] || fallback || section;
}

/** Archive sources are keyed by bare source name (`jandan`), not by section. */
export function sourceLabel(source: string, fallback = ''): string {
  return sectionLabel(`web.${source}`, fallback);
}

const STATUS_LABELS: Record<string, string> = {
  pending: '排队中',
  running: '运行中',
  succeeded: '成功',
  failed: '失败',
  rejected: '已拒绝',
};

export function statusLabel(status: string): string {
  return STATUS_LABELS[status] || status;
}

const KIND_LABELS: Record<string, string> = {
  trigger_job: '手动',
  scheduled_job: '定时',
};

export function kindLabel(kind: string): string {
  return KIND_LABELS[kind] || kind;
}

/** Job-request targets are job keys, plus the special `all`. */
export function targetLabel(target: string): string {
  if (target === 'all') return '全部任务';
  return sourceLabel(target, target);
}
