import { useQuery, keepPreviousData } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { api } from '../api/client';
import type { ArchiveItem, ArchiveSourceStat, ListResponse, PagedResponse } from '../api/types';

const PAGE_SIZE = 50;

export function RecordsPage() {
  const [source, setSource] = useState('');
  const [search, setSearch] = useState('');
  const [query, setQuery] = useState('');
  const [offset, setOffset] = useState(0);

  const sources = useQuery({
    queryKey: ['archive-sources'],
    queryFn: () => api.get<ListResponse<ArchiveSourceStat>>('/api/v2/archive/sources'),
  });

  // Default to the first source that actually has rows.
  useEffect(() => {
    if (source || !sources.data) return;
    const withData = sources.data.items.find((item) => item.total > 0) ?? sources.data.items[0];
    if (withData) setSource(withData.source);
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
              onClick={() => {
                setSource(item.source);
                setOffset(0);
              }}
            >
              {item.name}
              <span className="badge">{item.total}</span>
            </button>
          ))}
        </div>

        <form
          className="inline-form"
          onSubmit={(event) => {
            event.preventDefault();
            setQuery(search.trim());
            setOffset(0);
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
                setQuery('');
                setOffset(0);
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
            <table className="table">
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
                    <td className="muted">{item.created_at ?? '—'}</td>
                  </tr>
                ))}
              </tbody>
            </table>

            <div className="pager">
              <button type="button" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}>
                上一页
              </button>
              <span className="muted">
                第 {page} / {pages} 页 · 共 {total} 条
              </span>
              <button type="button" disabled={offset + PAGE_SIZE >= total} onClick={() => setOffset(offset + PAGE_SIZE)}>
                下一页
              </button>
            </div>
          </>
        )}
      </section>
    </div>
  );
}
