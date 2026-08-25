import { CheckboxField, NumberField, Repeater, TextField } from './Field';
import { CookieCloudPicker } from './CookieCloudPicker';
import { list, patcher, str, type SectionFormProps } from './sectionFields';

export interface BilibiliFavorite {
  fav_id?: number;
  path?: string;
}

export interface BilibiliAccount {
  name?: string;
  favorites?: BilibiliFavorite[];
  toview_enabled?: boolean;
  toview_path?: string;
  /** Name of an entry in the shared `cookiecloud` section. */
  cookiecloud?: string;
}

const ACCOUNT_NAME_RE = /^[A-Za-z0-9_-]+$/;

interface FavoritesEditorProps {
  favorites: BilibiliFavorite[];
  accountName: string;
  onChange: (next: BilibiliFavorite[]) => void;
}

function FavoritesEditor({ favorites, accountName, onChange }: FavoritesEditorProps) {
  const update = (index: number, patch: Partial<BilibiliFavorite>) => {
    onChange(favorites.map((favorite, position) => (position === index ? { ...favorite, ...patch } : favorite)));
  };

  return (
    <Repeater
      label="收藏夹"
      count={favorites.length}
      addLabel="添加收藏夹"
      empty="该账号还没有收藏夹；若也没开稍后再看，任务会保持未就绪。"
      hint="收藏夹 ID 取自收藏夹地址里的 fid，每个收藏夹单独指定保存目录。"
      onAdd={() =>
        onChange([
          ...favorites,
          { fav_id: Number.NaN, path: `collection/bilibili/${accountName || 'account'}/fav` },
        ])
      }
    >
      <table className="table compact">
        <thead>
          <tr>
            <th>收藏夹 ID</th>
            <th>保存路径</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {favorites.map((favorite, index) => {
            const idOk = Number.isInteger(favorite.fav_id) && (favorite.fav_id ?? 0) > 0;
            return (
              // eslint-disable-next-line react/no-array-index-key -- rows have no stable key while being edited
              <tr key={index}>
                <td className="cell-id">
                  <NumberField
                    label=""
                    ariaLabel="收藏夹 ID"
                    value={favorite.fav_id ?? Number.NaN}
                    onChange={(next) => update(index, { fav_id: next })}
                    invalid={!idOk}
                    placeholder="123456789"
                  />
                  {!idOk && Number.isFinite(favorite.fav_id) && <p className="warn">必须是正整数</p>}
                </td>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={favorite.path ?? ''}
                    placeholder="collection/bilibili/xxx/fav"
                    onChange={(event) => update(index, { path: event.target.value })}
                  />
                </td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() => onChange(favorites.filter((_, position) => position !== index))}
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

export function BilibiliForm(props: SectionFormProps) {
  const set = patcher(props);
  const accounts = list<BilibiliAccount>(props.value, 'accounts');

  const update = (index: number, patch: Partial<BilibiliAccount>) => {
    set(
      'accounts',
      accounts.map((account, position) => (position === index ? { ...account, ...patch } : account)),
    );
  };

  return (
    <div className="field-grid">
      <TextField
        label="下载代理"
        value={str(props.value, 'proxy')}
        onChange={(next) => set('proxy', next)}
        mono
        wide
        placeholder="http://host:port"
        hint="仅用于 yt-dlp 视频下载，收藏夹 API 仍然直连。B 站按出口 IP 分配 CDN 镜像，海外机房直连有概率被分到单连接限速约 1MB/s 的 Akamai 镜像；走香港出口可稳定分到快的腾讯云镜像。留空直连。"
      />
      <Repeater
        label="账号"
        count={accounts.length}
        addLabel="添加账号"
        empty="还没有账号，Bilibili 任务会保持未就绪。"
        hint="每个账号引用一个 CookieCloud 配置——那个 vault 就是这个账号的身份。"
        onAdd={() =>
          set('accounts', [
            ...accounts,
            {
              name: '',
              favorites: [],
              toview_enabled: false,
              toview_path: 'collection/bilibili/toview',
              cookiecloud: '',
            },
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
                      const count = account.favorites?.length ?? 0;
                      if (window.confirm(`删除账号「${name || index + 1}」及其 ${count} 个收藏夹？`)) {
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
                    hint="仅限字母、数字、下划线、连字符；也是 cookie 文件名的一部分"
                    error={name && !nameOk ? '名称含非法字符' : undefined}
                  />
                </div>

                <div className="field-grid">
                  <CheckboxField
                    label="抓取稍后再看"
                    checked={account.toview_enabled ?? false}
                    onChange={(next) => update(index, { toview_enabled: next })}
                    hint="抓完会清空该账号的稍后再看列表"
                  />
                  <TextField
                    label="稍后再看保存路径"
                    value={account.toview_path ?? ''}
                    onChange={(next) => update(index, { toview_path: next })}
                    mono
                    placeholder="collection/bilibili/toview"
                  />
                </div>

                <FavoritesEditor
                  favorites={account.favorites ?? []}
                  accountName={name}
                  onChange={(next) => update(index, { favorites: next })}
                />

                <div className="subsection">
                  <h4>登录会话</h4>
                  <div className="field-grid">
                    <CookieCloudPicker
                      source="bilibili"
                      value={account.cookiecloud ?? ''}
                      onChange={(next) => update(index, { cookiecloud: next })}
                      hint="浏览器插件需要同步 bilibili.com 的登录 cookie。"
                    />
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </Repeater>
    </div>
  );
}

/** Client-side mirror of the Bilibili model's cross-field rules. */
export function validateBilibili(value: Record<string, unknown>): string[] {
  const issues: string[] = [];
  const accounts = list<BilibiliAccount>(value, 'accounts');
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

    const favorites = account.favorites ?? [];
    if (favorites.length === 0 && !account.toview_enabled) {
      issues.push(`账号 ${label}：至少添加一个收藏夹，或开启稍后再看`);
    }
    if (account.toview_enabled && !account.toview_path?.trim()) {
      issues.push(`账号 ${label}：开启稍后再看后必须填保存路径`);
    }

    const seenFavIds = new Set<number>();
    favorites.forEach((favorite, position) => {
      // A NaN id is already reported by the generic numeric scan.
      if (favorite.fav_id === undefined) {
        issues.push(`账号 ${label} 的收藏夹 #${position + 1}：ID 不能为空`);
      } else if (Number.isFinite(favorite.fav_id) && !(Number.isInteger(favorite.fav_id) && favorite.fav_id > 0)) {
        issues.push(`账号 ${label} 的收藏夹 #${position + 1}：ID 必须是正整数`);
      } else if (Number.isFinite(favorite.fav_id)) {
        if (seenFavIds.has(favorite.fav_id)) {
          issues.push(`账号 ${label}：收藏夹 ${favorite.fav_id} 重复`);
        }
        seenFavIds.add(favorite.fav_id);
      }
      if (!favorite.path?.trim()) {
        issues.push(`账号 ${label} 的收藏夹 #${position + 1}：保存路径不能为空`);
      }
    });
  });

  return issues;
}
