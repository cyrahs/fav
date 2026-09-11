import { CheckboxGroup, NumberField, Repeater, SecretField, TextField, type Option } from './Field';
import { list, num, patcher, str, type SectionFormProps } from './sectionFields';
import { WeChatLogin } from './WeChatLogin';

export type WeChatMediaType = 'video' | 'image' | 'file';

export interface WeChatAccount {
  name?: string;
  path?: string;
  media_types?: WeChatMediaType[];
  bot_token?: string;
  bot_id?: string;
  user_id?: string;
  base_url?: string;
  cdn_base_url?: string;
}

const MEDIA_TYPES: Option<WeChatMediaType>[] = [
  { value: 'video', label: '视频' },
  { value: 'image', label: '图片' },
  { value: 'file', label: '文件' },
];

const ACCOUNT_NAME_RE = /^[A-Za-z0-9_-]+$/;

export function WeChatForm(props: SectionFormProps) {
  const set = patcher(props);
  const accounts = list<WeChatAccount>(props.value, 'accounts');

  const update = (index: number, patch: Partial<WeChatAccount>) => {
    set(
      'accounts',
      accounts.map((account, position) => (position === index ? { ...account, ...patch } : account)),
    );
  };

  return (
    <div className="field-grid">
      <p className="field-hint field-wide">
        微信没有可订阅的频道：在微信里把图片、视频、文件发给（或转发给）绑定的 ClawBot 联系人，worker 就会收下并归档。视频会被微信压缩，要原片请“以文件形式发送”。
      </p>

      <details className="subsection">
        <summary>接收与下载参数</summary>
        <div className="field-grid">
          <NumberField
            label="长轮询超时（秒）"
            value={num(props.value, 'long_poll_timeout_seconds', 35)}
            onChange={(next) => set('long_poll_timeout_seconds', next)}
            step={0.5}
            hint="服务器最长挂起等待新消息的时间；服务器建议值会覆盖它"
          />
          <NumberField
            label="下载间隔（秒）"
            value={num(props.value, 'download_delay_seconds', 0)}
            onChange={(next) => set('download_delay_seconds', next)}
            step={0.5}
          />
          <NumberField
            label="会话失效后暂停（秒）"
            value={num(props.value, 'session_pause_seconds', 3600)}
            onChange={(next) => set('session_pause_seconds', next)}
            step={0.5}
            hint="服务器返回 -14 后停多久再试；期间需要重新扫码"
          />
          <NumberField
            label="单个媒体最多重试次数"
            value={num(props.value, 'max_download_attempts', 8)}
            onChange={(next) => set('max_download_attempts', next)}
          />
        </div>
      </details>

      <Repeater
        label="账号"
        count={accounts.length}
        addLabel="添加账号"
        empty="还没有账号，微信任务会保持未就绪。"
        hint="每个账号对应一个 ClawBot 机器人。改动账号后需要重启 worker，实时监听在进程启动时建立。"
        onAdd={() => set('accounts', [...accounts, { name: '', path: 'collection/wechat', media_types: ['video', 'image', 'file'] }])}
      >
        <div className="stack">
          {accounts.map((account, index) => {
            const name = account.name ?? '';
            const nameOk = ACCOUNT_NAME_RE.test(name);
            const mediaTypes = account.media_types ?? [];
            const bound = Boolean(account.bot_token);
            return (
              // eslint-disable-next-line react/no-array-index-key -- accounts are reorderable and unsaved rows have no id
              <div key={index} className="account-card">
                <div className="account-head">
                  <strong>
                    {name || `账号 #${index + 1}`}
                    {bound ? <span className="ok"> · 已绑定</span> : <span className="warn"> · 未绑定</span>}
                  </strong>
                  <button
                    type="button"
                    className="danger"
                    onClick={() => {
                      if (window.confirm(`删除账号「${name || index + 1}」？已收到的文件不会被删除。`)) {
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
                    hint="仅限字母、数字、下划线、连字符"
                    error={name && !nameOk ? '名称含非法字符' : undefined}
                  />
                  <TextField
                    label="保存路径"
                    value={str(account as Record<string, unknown>, 'path')}
                    onChange={(next) => update(index, { path: next })}
                    placeholder={`collection/wechat/${name || 'account'}`}
                    mono
                    invalid={!account.path?.trim()}
                  />
                  <CheckboxGroup
                    label="媒体类型"
                    values={mediaTypes}
                    options={MEDIA_TYPES}
                    onChange={(next) => update(index, { media_types: next })}
                    error={mediaTypes.length === 0 ? '至少选一种' : undefined}
                  />
                  <SecretField
                    label="bot_token"
                    value={account.bot_token ?? ''}
                    onChange={(next) => update(index, { bot_token: next })}
                    hint={bound ? undefined : '通过下方扫码获得；也可以粘贴其他 iLink 客户端导出的 token'}
                  />
                  <TextField
                    label="bot_id"
                    value={account.bot_id ?? ''}
                    onChange={(next) => update(index, { bot_id: next })}
                    mono
                    hint="扫码后自动填写"
                  />
                  <TextField
                    label="允许的发送者（user_id）"
                    value={account.user_id ?? ''}
                    onChange={(next) => update(index, { user_id: next })}
                    mono
                    hint="扫码后自动填写为扫码的微信；留空则接收任何发送者"
                  />
                  <TextField
                    label="API 地址"
                    value={str(account as Record<string, unknown>, 'base_url', 'https://ilinkai.weixin.qq.com')}
                    onChange={(next) => update(index, { base_url: next })}
                    mono
                  />
                  <TextField
                    label="CDN 地址"
                    value={str(account as Record<string, unknown>, 'cdn_base_url', 'https://novac2c.cdn.weixin.qq.com/c2c')}
                    onChange={(next) => update(index, { cdn_base_url: next })}
                    mono
                  />
                </div>

                <WeChatLogin
                  account={name}
                  path={account.path ?? ''}
                  mediaTypes={mediaTypes}
                  onBound={(stored) => update(index, stored as Partial<WeChatAccount>)}
                />
              </div>
            );
          })}
        </div>
      </Repeater>
    </div>
  );
}

/** Client-side mirror of the WeChat model's cross-field rules. */
export function validateWeChat(value: Record<string, unknown>): string[] {
  const issues: string[] = [];
  const accounts = list<WeChatAccount>(value, 'accounts');
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
    if (!account.path?.trim()) {
      issues.push(`账号 ${label}：保存路径不能为空`);
    }
    if ((account.media_types ?? []).length === 0) {
      issues.push(`账号 ${label}：至少选择一种媒体类型`);
    }
    for (const key of ['base_url', 'cdn_base_url'] as const) {
      const url = account[key];
      if (typeof url === 'string' && url.trim() && !/^https?:\/\//.test(url.trim())) {
        issues.push(`账号 ${label}：${key} 必须以 http:// 或 https:// 开头`);
      }
    }
  });

  return issues;
}
