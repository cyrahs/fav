import { CheckboxGroup, NumberField, Repeater, SecretField, TextField, type Option } from './Field';
import { list, num, patcher, str, type SectionFormProps } from './sectionFields';

export type TelegramMediaType = 'video' | 'image';

export interface TelegramChannel {
  id?: number;
  path?: string;
  media_types?: TelegramMediaType[];
}

export interface TelegramAccount {
  name?: string;
  api_id?: number;
  api_hash?: string;
  session_path?: string;
  channels?: TelegramChannel[];
}

const MEDIA_TYPES: Option<TelegramMediaType>[] = [
  { value: 'video', label: '视频' },
  { value: 'image', label: '图片' },
];

const ACCOUNT_NAME_RE = /^[A-Za-z0-9_-]+$/;

/** A Bot API id (-100…) is accepted and normalized server-side; anything else must be positive. */
function isValidChannelId(id: number | undefined): boolean {
  if (id === undefined || !Number.isFinite(id) || !Number.isInteger(id)) {
    return false;
  }
  return id > 0 || String(id).startsWith('-100');
}

interface ChannelsEditorProps {
  channels: TelegramChannel[];
  accountName: string;
  onChange: (next: TelegramChannel[]) => void;
}

function ChannelsEditor({ channels, accountName, onChange }: ChannelsEditorProps) {
  const update = (index: number, patch: Partial<TelegramChannel>) => {
    onChange(channels.map((channel, position) => (position === index ? { ...channel, ...patch } : channel)));
  };

  return (
    <Repeater
      label="频道"
      count={channels.length}
      addLabel="添加频道"
      empty="该账号还没有频道，任务会保持未就绪。"
      hint="频道 ID 可填 Telethon 的正数 ID，或 Bot API 的 -100 开头 ID（保存时自动换算）。"
      onAdd={() =>
        onChange([...channels, { id: Number.NaN, path: `collection/telegram/${accountName || 'account'}`, media_types: ['video'] }])
      }
    >
      <table className="table compact">
        <thead>
          <tr>
            <th>频道 ID</th>
            <th>保存路径</th>
            <th>媒体类型</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {channels.map((channel, index) => {
            const mediaTypes = channel.media_types ?? [];
            const idOk = isValidChannelId(channel.id);
            return (
              // eslint-disable-next-line react/no-array-index-key -- rows have no stable key while being edited
              <tr key={index}>
                <td className="cell-id">
                  <NumberField
                    label=""
                    ariaLabel="频道 ID"
                    value={channel.id ?? Number.NaN}
                    onChange={(next) => update(index, { id: next })}
                    invalid={!idOk}
                    placeholder="-1001234567890"
                  />
                  {!idOk && Number.isFinite(channel.id) && <p className="warn">ID 必须为正数或 -100 开头</p>}
                </td>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={channel.path ?? ''}
                    placeholder="collection/telegram/xxx"
                    onChange={(event) => update(index, { path: event.target.value })}
                  />
                </td>
                <td className="cell-media">
                  <CheckboxGroup
                    label=""
                    values={mediaTypes}
                    options={MEDIA_TYPES}
                    onChange={(next) => update(index, { media_types: next })}
                    error={mediaTypes.length === 0 ? '至少选一种' : undefined}
                  />
                </td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() => onChange(channels.filter((_, position) => position !== index))}
                  >
                    删除
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </Repeater>
  );
}

export function TelegramForm(props: SectionFormProps) {
  const set = patcher(props);
  const accounts = list<TelegramAccount>(props.value, 'accounts');

  const update = (index: number, patch: Partial<TelegramAccount>) => {
    set(
      'accounts',
      accounts.map((account, position) => (position === index ? { ...account, ...patch } : account)),
    );
  };

  return (
    <div className="field-grid">
      <details className="subsection">
        <summary>抓取节流参数</summary>
        <div className="field-grid">
          <NumberField
            label="每轮扫描消息数"
            value={num(props.value, 'scan_limit', 50)}
            onChange={(next) => set('scan_limit', next)}
            hint="字段 scan_limit"
          />
          <NumberField
            label="每频道单轮下载上限"
            value={num(props.value, 'download_limit_per_channel', 2)}
            onChange={(next) => set('download_limit_per_channel', next)}
          />
          <NumberField
            label="下载间隔（秒）"
            value={num(props.value, 'download_delay_seconds', 60)}
            onChange={(next) => set('download_delay_seconds', next)}
            step={0.5}
          />
          <NumberField
            label="频道冷却（秒）"
            value={num(props.value, 'channel_cooldown_seconds', 1800)}
            onChange={(next) => set('channel_cooldown_seconds', next)}
            step={0.5}
          />
          <NumberField
            label="历史翻页等待（秒）"
            value={num(props.value, 'history_wait_seconds', 1)}
            onChange={(next) => set('history_wait_seconds', next)}
            step={0.5}
          />
          <NumberField
            label="FloodWait 阈值（秒）"
            value={num(props.value, 'flood_sleep_threshold_seconds', 300)}
            onChange={(next) => set('flood_sleep_threshold_seconds', next)}
            hint="超过该值的 FloodWait 直接抛错而不是等待"
          />
        </div>
      </details>

      <Repeater
        label="账号"
        count={accounts.length}
        addLabel="添加账号"
        empty="还没有账号，Telegram 任务会保持未就绪。"
        hint="改动账号后需要重启 worker，实时监听在进程启动时建立。"
        onAdd={() =>
          set('accounts', [
            ...accounts,
            { name: '', api_id: Number.NaN, api_hash: '', session_path: './data/telethon-session', channels: [] },
          ])
        }
      >
        <div className="stack">
          {accounts.map((account, index) => {
            const name = account.name ?? '';
            const nameOk = ACCOUNT_NAME_RE.test(name);
            return (
              // eslint-disable-next-line react/no-array-index-key -- accounts are reorderable and unsaved rows have no id
              <div key={index} className="account-card">
                <div className="account-head">
                  <strong>{name || `账号 #${index + 1}`}</strong>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => {
                      if (window.confirm(`删除账号「${name || index + 1}」及其 ${account.channels?.length ?? 0} 个频道？`)) {
                        set(
                          'accounts',
                          accounts.filter((_, position) => position !== index),
                        );
                      }
                    }}
                  >
                    删除账号
                  </button>
                </div>

                <div className="field-grid">
                  <TextField
                    label="名称"
                    value={name}
                    onChange={(next) => update(index, { name: next })}
                    mono
                    invalid={!nameOk}
                    hint="仅限字母、数字、下划线、连字符；也是 session 文件名的一部分"
                    error={name && !nameOk ? '名称含非法字符' : undefined}
                  />
                  <NumberField
                    label="api_id"
                    value={account.api_id ?? Number.NaN}
                    onChange={(next) => update(index, { api_id: next })}
                  />
                  <SecretField
                    label="api_hash"
                    value={account.api_hash ?? ''}
                    onChange={(next) => update(index, { api_hash: next })}
                  />
                  <TextField
                    label="Session 路径"
                    value={str(account as Record<string, unknown>, 'session_path')}
                    onChange={(next) => update(index, { session_path: next })}
                    mono
                  />
                </div>

                <ChannelsEditor
                  channels={account.channels ?? []}
                  accountName={name}
                  onChange={(next) => update(index, { channels: next })}
                />
              </div>
            );
          })}
        </div>
      </Repeater>
    </div>
  );
}

/** Client-side mirror of the Telegram model's cross-field rules. */
export function validateTelegram(value: Record<string, unknown>): string[] {
  const issues: string[] = [];
  const accounts = list<TelegramAccount>(value, 'accounts');
  const seenNames = new Set<string>();

  accounts.forEach((account, index) => {
    const label = account.name || `#${index + 1}`;
    const name = account.name ?? '';
    if (!ACCOUNT_NAME_RE.test(name)) {
      issues.push(`账号 ${label}：名称只能包含字母、数字、下划线、连字符`);
    } else {
      const folded = name.toLowerCase();
      if (seenNames.has(folded)) {
        issues.push(`账号名重复：${name}`);
      }
      seenNames.add(folded);
    }

    const routes = new Set<string>();
    (account.channels ?? []).forEach((channel, position) => {
      // A NaN id is already reported by the generic numeric scan; only add a
      // message for the cases that scan cannot see.
      if (channel.id === undefined) {
        issues.push(`账号 ${label} 的频道 #${position + 1}：ID 不能为空`);
      } else if (Number.isFinite(channel.id) && !isValidChannelId(channel.id)) {
        issues.push(`账号 ${label} 的频道 #${position + 1}：ID 必须是正数或 -100 开头的整数`);
      }
      if (!channel.path?.trim()) {
        issues.push(`账号 ${label} 的频道 #${position + 1}：保存路径不能为空`);
      }
      const mediaTypes = channel.media_types ?? [];
      if (mediaTypes.length === 0) {
        issues.push(`账号 ${label} 的频道 #${position + 1}：至少选择一种媒体类型`);
      }
      for (const mediaType of mediaTypes) {
        const route = `${channel.id}:${mediaType}`;
        if (routes.has(route)) {
          issues.push(`账号 ${label}：频道 ${channel.id} 的 ${mediaType} 被重复路由`);
        }
        routes.add(route);
      }
    });
  });

  return issues;
}
