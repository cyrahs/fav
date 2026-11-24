import { useMutation } from '@tanstack/react-query';
import { ApiError, api } from '../api/client';
import type { ZhihuCollectionListed, ZhihuCollectionOwner, ZhihuCollectionsResponse } from '../api/types';
import { NumberField } from './Field';
import { list, num, patcher, type SectionFormProps } from './sectionFields';

export interface ZhihuCollection {
  id?: number;
  title?: string;
  owner?: ZhihuCollectionOwner;
  path?: string;
}

const GROUPS: { owner: ZhihuCollectionOwner; title: string; empty: string }[] = [
  { owner: 'mine', title: '我的收藏夹', empty: '这个账号还没有自己的收藏夹。' },
  { owner: 'followed', title: '我关注的收藏夹', empty: '这个账号没有关注别人的收藏夹。' },
];

const ERROR_LABELS: Record<string, string> = {
  zhihu_login: '知乎还没登录（或登录已失效），先到「登录态」页登录知乎',
  zhihu_unavailable: '暂时读不到知乎',
};

/** A folder name from a collection title: no path separators, no leading dots. */
function folderName(title: string, id: number): string {
  const cleaned = title.replace(/[\\/:*?"<>|]/g, '_').replace(/^\.+/, '').trim();
  return cleaned || String(id);
}

export function defaultZhihuPath(title: string, id: number): string {
  return `collection/zhihu/${folderName(title, id)}`;
}

interface Row {
  id: number;
  title: string;
  owner: ZhihuCollectionOwner;
  detail: string;
}

function detailOf(listed: ZhihuCollectionListed): string {
  const parts = [`${listed.item_count} 条`];
  if (!listed.is_public) {
    parts.push('私密');
  }
  if (listed.owner === 'followed' && listed.creator) {
    parts.push(`创建者 ${listed.creator}`);
  }
  return parts.join(' · ');
}

/**
 * Pick collections to crawl. The list comes from the signed-in 知乎 profile, read on
 * demand because it opens a browser; ticked collections stay listed (from the saved
 * settings) whether or not the list has been read, so one can always be unticked.
 */
export function ZhihuForm(props: SectionFormProps) {
  const set = patcher(props);
  const chosen = list<ZhihuCollection>(props.value, 'collections');
  const read = useMutation({
    mutationFn: () => api.get<ZhihuCollectionsResponse>('/api/v2/zhihu/collections'),
  });
  const listed = read.data?.collections ?? [];

  const rows = (owner: ZhihuCollectionOwner): Row[] => {
    const fromList = listed
      .filter((collection) => collection.owner === owner)
      .map((collection) => ({ id: collection.id, title: collection.title, owner, detail: detailOf(collection) }));
    const listedIds = new Set(listed.map((collection) => collection.id));
    const onlyChosen = chosen
      .filter((collection) => (collection.owner ?? 'mine') === owner && !listedIds.has(collection.id ?? Number.NaN))
      .map((collection) => ({
        id: collection.id ?? Number.NaN,
        title: collection.title || String(collection.id),
        owner,
        detail: read.data ? '知乎列表里已没有它' : '',
      }));
    return [...fromList, ...onlyChosen];
  };

  const toggle = (row: Row, on: boolean) => {
    if (on) {
      set('collections', [
        ...chosen,
        { id: row.id, title: row.title, owner: row.owner, path: defaultZhihuPath(row.title, row.id) },
      ]);
    } else {
      set(
        'collections',
        chosen.filter((collection) => collection.id !== row.id),
      );
    }
  };

  const setPath = (id: number, path: string) => {
    set(
      'collections',
      chosen.map((collection) => (collection.id === id ? { ...collection, path } : collection)),
    );
  };

  const error = read.error;
  const errorText =
    error instanceof ApiError
      ? `${ERROR_LABELS[error.code] ?? '读取失败'}：${error.message}`
      : error
        ? (error as Error).message
        : '';

  return (
    <div className="field-grid">
      <div className="subsection">
        <div className="cc-test">
          <button type="button" className="ghost" disabled={read.isPending} onClick={() => read.mutate()}>
            {read.isPending ? '读取中…（要开一次浏览器）' : read.data ? '重新读取' : '从知乎读取收藏夹'}
          </button>
          {read.data && <span className="ok">✓ {read.data.account}</span>}
          {errorText && <span className="warn">{errorText}</span>}
        </div>
        <p className="muted">
          用「登录态」页登录的知乎账号读取。勾选要抓的收藏夹，每个收藏夹单独指定保存目录；抓取回答、文章、想法和视频里的图片和视频，已经存过的帖子（包括从 Telegram、微信发来过的链接）不会重复下载。
        </p>
      </div>

      {GROUPS.map((group) => {
        const groupRows = rows(group.owner);
        return (
          <div key={group.owner} className="subsection">
            <h4>{group.title}</h4>
            {groupRows.length === 0 ? (
              <p className="muted">{read.data ? group.empty : '还没读取，也没有勾选的收藏夹。'}</p>
            ) : (
              <table className="table compact">
                <thead>
                  <tr>
                    <th>抓取</th>
                    <th>收藏夹</th>
                    <th>保存路径</th>
                  </tr>
                </thead>
                <tbody>
                  {groupRows.map((row) => {
                    const entry = chosen.find((collection) => collection.id === row.id);
                    return (
                      <tr key={row.id}>
                        <td>
                          <input
                            type="checkbox"
                            aria-label={`抓取 ${row.title}`}
                            checked={entry !== undefined}
                            onChange={(event) => toggle(row, event.target.checked)}
                          />
                        </td>
                        <td>
                          <a href={`https://www.zhihu.com/collection/${row.id}`} target="_blank" rel="noreferrer">
                            {row.title}
                          </a>
                          {row.detail && <div className="muted">{row.detail}</div>}
                        </td>
                        <td>
                          {entry && (
                            <input
                              type="text"
                              className="mono-input"
                              aria-label={`${row.title} 的保存路径`}
                              value={entry.path ?? ''}
                              placeholder={defaultZhihuPath(row.title, row.id)}
                              onChange={(event) => setPath(row.id, event.target.value)}
                            />
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
          </div>
        );
      })}

      <NumberField
        label="每次最多下载"
        value={num(props.value, 'max_posts_per_run', 100)}
        onChange={(next) => set('max_posts_per_run', next)}
        hint="一次运行最多下载多少条带图片或视频的帖子（所有收藏夹合计），剩下的下次接着下。"
      />
    </div>
  );
}

/** Client-side mirror of the Zhihu model's rules. */
export function validateZhihu(value: Record<string, unknown>): string[] {
  const issues: string[] = [];
  const seen = new Set<number>();
  list<ZhihuCollection>(value, 'collections').forEach((collection, index) => {
    const label = collection.title || `#${index + 1}`;
    if (!(Number.isInteger(collection.id) && (collection.id ?? 0) > 0)) {
      issues.push(`收藏夹 ${label}：ID 必须是正整数`);
    } else if (seen.has(collection.id as number)) {
      issues.push(`收藏夹 ${label} 重复`);
    } else {
      seen.add(collection.id as number);
    }
    if (!collection.path?.trim()) {
      issues.push(`收藏夹 ${label}：保存路径不能为空`);
    }
  });
  const max = num(value, 'max_posts_per_run', 100);
  if (!(Number.isInteger(max) && max > 0)) {
    issues.push('每次最多下载必须是正整数');
  }
  return issues;
}
