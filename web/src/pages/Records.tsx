import { useQuery, keepPreviousData } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { api } from '../api/client';
import type { ArchiveItem, ArchiveSourceStat, ListResponse, PagedResponse } from '../api/types';
import { formatDateTime } from '../format';
import { sourceLabel } from '../labels';

const PAGE_SIZE = 50;

export function RecordsPage() {
  // Source, query and offset live in the URL, so a refresh keeps the position
  // and a filtered view can be shared as a link.
  const [searchParams, setSearchParams] = useSearchParams();
  const source = searchParams.get('source') ?? '';
  const query = searchParams.get('q') ?? '';
  const offsetRaw = Number(searchParams.get('offset') ?? '0');
  const offset = Number.isInteger(offsetRaw) && offsetRaw > 0 ? offsetRaw : 0;
  const [search, setSearch] = useState(query);

  const apply = (next: { source: string; q: string; offset: number }, replace = false) => {
    const params: Record<string, string> = { source: next.source };
    if (next.q) params.q = next.q;
    if (next.offset > 0) params.offset = String(next.offset);
    setSearchParams(params, { replace });
  };

  // Keep the input in sync when the query comes from the URL (back button, shared link).
  useEffect(() => {
    setSearch(query);
  }, [query]);

  const sources = useQuery({
    queryKey: ['archive-sources'],
    queryFn: () => api.get<ListResponse<ArchiveSourceStat>>('/api/v2/archive/sources'),
  });

  // Default to the first source that actually has rows.
  useEffect(() => {
    if (source || !sources.data) return;
    const withData = sources.data.items.find((item) => item.total > 0) ?? sources.data.items[0];
    if (withData) apply({ source: withData.source, q: query, offset: 0 }, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- apply/query are stable enough here
  }, [source, sources.data]);

  const params = new URLSearchParams({ source, limit: String(PAGE_SIZE), offset: String(offset) });
  if (query) params.set('q', query);

  const items = useQuery({
    queryKey: ['archive-items', source, query, offset],
    queryFn: () => api.get<PagedResponse<ArchiveItem>>(`/api/v2/archive/items?${params.toString()}`),
    enabled: Boolean(source),
    placeholderData: keepPreviousData,
  });

  const total = items.data?.total ?? 0;
  const page = Math.floor(offset / PAGE_SIZE) + 1;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  // With keepPreviousData the old page stays visible during a fetch; dim it so
  // paging and re-searching give immediate feedback.
  const refetching = items.isFetching && !items.isLoading;

  return (
    <div className="stack">
      <section className="card">
        <h2>归档记录</h2>
        <div className="source-tabs">
          {sources.data?.items.map((item) => (
            <button
              key={item.source}
              type="button"
              className={item.source === source ? 'tab tab-active' : 'tab'}
              aria-pressed={item.source === source}
              onClick={() => apply({ source: item.source, q: query, offset: 0 })}
            >
              {sourceLabel(item.source, item.name)}
              <span className="badge">{item.total}</span>
            </button>
          ))}
        </div>

        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            apply({ source, q: search.trim(), offset: 0 });
          }}
        >
          <input
            type="search"
            value={search}
            placeholder="搜索标题、作者、ID…"
            onChange={(event) => setSearch(event.target.value)}
          />
          <button type="submit">搜索</button>
          {query && (
            <button
              type="button"
              className="ghost"
              onClick={() => {
                setSearch('');
                apply({ source, q: '', offset: 0 });
              }}
            >
              清除
            </button>
          )}
        </form>

        {items.isLoading && <p>加载中…</p>}
        {items.error && <p className="warn">{(items.error as Error).message}</p>}
        {items.data?.items.length === 0 && <p className="muted">没有匹配的记录。</p>}

        {items.data && items.data.items.length > 0 && (
          <>
            <table className={refetching ? 'table refetching' : 'table'}>
              <thead>
                <tr>
                  <th>标题</th>
                  <th>信息</th>
                  <th>归档时间</th>
                </tr>
              </thead>
              <tbody>
                {items.data.items.map((item) => (
                  <tr key={item.id}>
                    <td>
                      {item.url ? (
                        <a href={item.url} target="_blank" rel="noreferrer">
                          {item.title || item.id}
                        </a>
                      ) : (
                        item.title || item.id
                      )}
                      <div className="muted mono">{item.id}</div>
                    </td>
                    <td className="muted">{item.subtitle || '—'}</td>
                    <td className="muted nowrap">{formatDateTime(item.created_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>

            <div className="pager">
              <button
                type="button"
                disabled={offset === 0 || items.isFetching}
                onClick={() => apply({ source, q: query, offset: Math.max(0, offset - PAGE_SIZE) })}
              >
                上一页
              </button>
              <span className="muted">
                第 {page} / {pages} 页 · 共 {total} 条
              </span>
              <button
                type="button"
                disabled={offset + PAGE_SIZE >= total || items.isFetching}
                onClick={() => apply({ source, q: query, offset: offset + PAGE_SIZE })}
              >
                下一页
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
