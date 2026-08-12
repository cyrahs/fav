import type { ReactElement } from 'react';
import { describeCron } from './CronInput';
import {
  CheckboxField,
  CheckboxGroup,
  NumberField,
  Repeater,
  SecretField,
  SelectField,
  TextField,
  type Option,
} from './Field';
import { BilibiliForm, validateBilibili } from './BilibiliForm';
import { TelegramForm, validateTelegram } from './TelegramForm';
import {
  PathField,
  bool,
  list,
  num,
  patcher,
  record,
  str,
  type SectionFormProps,
} from './sectionFields';

function NikkeForm(props: SectionFormProps) {
  const set = patcher(props);
  return (
    <div className="field-grid">
      <PathField {...props} />
      <div className="field-checks">
        <CheckboxField
          label="启用运行时图层抓取"
          checked={bool(props.value, 'runtime_capture_enabled')}
          onChange={(next) => set('runtime_capture_enabled', next)}
          hint="需要 Playwright Chromium"
        />
        <CheckboxField
          label="强制刷新抓取缓存"
          checked={bool(props.value, 'runtime_capture_force_refresh')}
          onChange={(next) => set('runtime_capture_force_refresh', next)}
        />
      </div>
      <NumberField
        label="抓取超时（秒）"
        value={num(props.value, 'runtime_capture_timeout_seconds', 60)}
        onChange={(next) => set('runtime_capture_timeout_seconds', next)}
        step={0.5}
      />
    </div>
  );
}

const HANIME1_PERIODS: Option<string>[] = [
  { value: 'weekly', label: '周榜' },
  { value: 'monthly', label: '月榜' },
];

function Hanime1Form(props: SectionFormProps) {
  const set = patcher(props);
  const ranking = record(props.value, 'ranking');
  const setRanking = (key: string, next: unknown) => set('ranking', { ...ranking, [key]: next });
  const periods = list<string>(ranking, 'periods');
  const deepScan = record(ranking, 'deep_scan');
  const setDeepScan = (key: string, next: unknown) => setRanking('deep_scan', { ...deepScan, [key]: next });

  return (
    <div className="field-grid">
      <PathField {...props} />
      <TextField label="站点地址" value={str(props.value, 'host')} onChange={(next) => set('host', next)} mono />
      <SelectField
        label="字幕语言"
        value={str(props.value, 'user_lang', 'zhs')}
        options={[
          { value: 'zhs', label: '简体中文 (zhs)' },
          { value: 'zht', label: '繁體中文 (zht)' },
        ]}
        onChange={(next) => set('user_lang', next)}
      />

      <div className="subsection">
        <h4>排行榜抓取</h4>
        <div className="field-grid">
          <CheckboxField
            label="启用排行榜"
            checked={bool(ranking, 'enabled')}
            onChange={(next) => setRanking('enabled', next)}
          />
          <CheckboxGroup
            label="榜单周期"
            values={periods}
            options={HANIME1_PERIODS}
            onChange={(next) => setRanking('periods', next)}
            error={periods.length === 0 ? '至少选择一个周期' : undefined}
          />
          <NumberField
            label="每个榜单抓取页数"
            value={num(ranking, 'pages', 1)}
            onChange={(next) => setRanking('pages', next)}
          />
        </div>
      </div>

      <div className="subsection">
        <h4>深度补扫</h4>
        <div className="field-grid">
          <CheckboxField
            label="新系列不足时向后补扫"
            checked={bool(deepScan, 'enabled')}
            onChange={(next) => setDeepScan('enabled', next)}
          />
          <NumberField
            label="每次扫描目标新系列数（可为小数）"
            value={num(deepScan, 'quota', 0.25)}
            onChange={(next) => setDeepScan('quota', next)}
            step={0.25}
          />
          <NumberField
            label="补扫最多额外页数"
            value={num(deepScan, 'max_extra_pages', 5)}
            onChange={(next) => setDeepScan('max_extra_pages', next)}
          />
        </div>
      </div>
    </div>
  );
}

const JANDAN_FAV_TYPES: Option<number>[] = [
  { value: 1, label: '无聊图 (wuliao)' },
  { value: 2, label: '随手拍 (snapshot)' },
  { value: 6, label: '妹子图 (girls)' },
];

function JandanForm(props: SectionFormProps) {
  const set = patcher(props);
  const favTypes = list<number>(props.value, 'fav_types');
  return (
    <div className="field-grid">
      <NumberField
        label="用户 ID"
        value={num(props.value, 'user_id')}
        onChange={(next) => set('user_id', next)}
        hint="字段 user_id"
      />
      <TextField label="API 地址" value={str(props.value, 'api_url')} onChange={(next) => set('api_url', next)} mono />
      <PathField {...props} />
      <CheckboxGroup
        label="收藏分类"
        values={favTypes}
        options={JANDAN_FAV_TYPES}
        onChange={(next) => set('fav_types', next)}
        hint="括号内是落盘的子目录名"
        error={favTypes.length === 0 ? '至少选择一个分类' : undefined}
      />
      <NumberField
        label="单次抓取上限"
        value={num(props.value, 'fav_num_limit', 45)}
        onChange={(next) => set('fav_num_limit', next)}
        hint="字段 fav_num_limit"
      />
    </div>
  );
}

interface KemonoCreator {
  service?: string;
  id?: string;
  name?: string;
}

function KemonoForm(props: SectionFormProps) {
  const set = patcher(props);
  const creators = list<KemonoCreator>(props.value, 'creators');

  const update = (index: number, patch: Partial<KemonoCreator>) => {
    set(
      'creators',
      creators.map((creator, position) => (position === index ? { ...creator, ...patch } : creator)),
    );
  };

  return (
    <div className="field-grid">
      <PathField {...props} />

      <Repeater
        label="创作者"
        count={creators.length}
        addLabel="添加创作者"
        empty="还没有创作者，Kemono 任务会保持未就绪。"
        hint="service 与 id 取自作品页地址：/{service}/user/{id}"
        onAdd={() => set('creators', [...creators, { service: 'fanbox', id: '', name: '' }])}
      >
        <table className="table compact">
          <thead>
            <tr>
              <th>Service</th>
              <th>用户 ID</th>
              <th>名称（目录名）</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {creators.map((creator, index) => (
              // eslint-disable-next-line react/no-array-index-key -- rows have no stable id until saved
              <tr key={index}>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={creator.service ?? ''}
                    placeholder="fanbox"
                    onChange={(event) => update(index, { service: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={creator.id ?? ''}
                    onChange={(event) => update(index, { id: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="text"
                    value={creator.name ?? ''}
                    onChange={(event) => update(index, { name: event.target.value })}
                  />
                </td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() =>
                      set(
                        'creators',
                        creators.filter((_, position) => position !== index),
                      )
                    }
                  >
                    删除
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Repeater>
    </div>
  );
}

function TelegramNotificationForm(props: SectionFormProps) {
  const set = patcher(props);
  const threadId = props.value.message_thread_id;
  const hasThread = threadId !== null && threadId !== undefined;

  return (
    <div className="field-grid">
      <CheckboxField
        label="启用通知投递"
        checked={bool(props.value, 'enabled')}
        onChange={(next) => set('enabled', next)}
        hint="关闭后通知仍会入队，只是不投递"
      />
      <SecretField
        label="Bot Token"
        value={str(props.value, 'bot_token')}
        onChange={(next) => set('bot_token', next)}
        hint="从 @BotFather 获取"
      />
      <TextField
        label="Chat ID"
        value={str(props.value, 'chat_id')}
        onChange={(next) => set('chat_id', next)}
        mono
        placeholder="-1001234567890 或 @channelname"
        hint="机器人必须已经在这个对话里"
      />
      <div className="field-checks">
        <CheckboxField
          label="发到话题（Topic）"
          checked={hasThread}
          onChange={(next) => set('message_thread_id', next ? 1 : null)}
          hint="仅适用于开启了话题的超级群"
        />
      </div>
      {hasThread && (
        <NumberField
          label="Message Thread ID"
          value={num(props.value, 'message_thread_id', 1)}
          onChange={(next) => set('message_thread_id', next)}
        />
      )}
    </div>
  );
}

export const SECTION_FORMS: Record<string, (props: SectionFormProps) => ReactElement> = {
  'web.bilibili': BilibiliForm,
  'web.telegram': TelegramForm,
  'web.nikke': NikkeForm,
  'web.hanime1': Hanime1Form,
  'web.jandan': JandanForm,
  'web.kemono': KemonoForm,
  'notifications.telegram': TelegramNotificationForm,
};

/**
 * Sections with nothing to configure beyond cron/enabled, which live on the jobs
 * page. Their remaining field is `path`; it keeps its default and is reachable
 * through `PUT /api/v2/settings/{section}` if a deployment ever needs to move it.
 */
export const JOBS_PAGE_ONLY_SECTIONS: ReadonlySet<string> = new Set([
  'web.stellasora',
  'web.bd2',
  'web.azurlane',
]);

/* ---------- client-side validation ---------- */

/** Find every NaN a numeric input left behind, so the save button can block on it. */
function collectNaNPaths(value: unknown, path = ''): string[] {
  if (typeof value === 'number') {
    return Number.isFinite(value) ? [] : [path || '(值)'];
  }
  if (Array.isArray(value)) {
    return value.flatMap((item, index) => collectNaNPaths(item, `${path}[${index}]`));
  }
  if (value && typeof value === 'object') {
    return Object.entries(value).flatMap(([key, item]) => collectNaNPaths(item, path ? `${path}.${key}` : key));
  }
  return [];
}

/**
 * Mirror the constraints most likely to be tripped in the form so the user gets a
 * message before the round trip. The backend stays the authority — anything not
 * caught here still comes back as a 422.
 */
export function validateSection(section: string, value: Record<string, unknown>): string[] {
  const issues = collectNaNPaths(value).map((path) => `${path} 不是合法数字`);

  if (typeof value.cron === 'string') {
    const described = describeCron(value.cron);
    if (!described.valid) {
      issues.push(`cron：${described.text}`);
    }
  }

  if (section === 'web.bilibili') {
    issues.push(...validateBilibili(value));
  }

  if (section === 'web.telegram') {
    issues.push(...validateTelegram(value));
  }

  if (section === 'web.jandan' && list<number>(value, 'fav_types').length === 0) {
    issues.push('收藏分类至少选择一个');
  }

  if (section === 'web.hanime1') {
    const ranking = record(value, 'ranking');
    if (list<string>(ranking, 'periods').length === 0) {
      issues.push('排行榜周期至少选择一个');
    }
    if (num(ranking, 'pages', 1) < 1) {
      issues.push('排行榜页数至少为 1');
    }
    const deepScan = record(ranking, 'deep_scan');
    const quota = num(deepScan, 'quota', 0.25);
    if (!Number.isFinite(quota) || quota <= 0) {
      issues.push('深度补扫目标新系列数必须大于 0');
    }
    if (num(deepScan, 'max_extra_pages', 5) < 1) {
      issues.push('深度补扫额外页数至少为 1');
    }
  }

  if (section === 'notifications.telegram') {
    const enabled = value.enabled === true;
    if (enabled && !str(value, 'bot_token').trim()) {
      issues.push('启用后必须填 Bot Token');
    }
    if (enabled && !str(value, 'chat_id').trim()) {
      issues.push('启用后必须填 Chat ID');
    }
    const thread = value.message_thread_id;
    if (thread !== null && thread !== undefined && !(Number.isInteger(thread) && (thread as number) > 0)) {
      issues.push('Message Thread ID 必须是正整数');
    }
  }

  if (section === 'web.kemono') {
    list<KemonoCreator>(value, 'creators').forEach((creator, index) => {
      if (!creator.service?.trim() || !creator.id?.trim() || !creator.name?.trim()) {
        issues.push(`创作者 #${index + 1} 的 service / id / 名称都不能为空`);
      }
    });
  }

  return issues;
}
