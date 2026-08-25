import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useRef, useState } from 'react';
import { useLocation } from 'react-router-dom';
import { api, ApiError } from '../api/client';
import type { Hanime1Author, Hanime1Seed, ListResponse, SettingsSection } from '../api/types';
import { TelegramNotificationTest } from '../components/TelegramNotificationTest';
import { JOBS_PAGE_ONLY_SECTIONS, SECTION_FORMS, validateSection } from '../components/sectionForms';
import { formatDateTime } from '../format';
import { sectionLabel } from '../labels';

/**
 * Each section gets a typed form from SECTION_FORMS; the raw-JSON editor stays
 * behind a toggle as an escape hatch for anything the form does not model (and
 * as the fallback if the backend grows a section this build predates).
 */
function SectionEditor({ section }: { section: SettingsSection }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<Record<string, unknown>>(section.value);
  const [rawMode, setRawMode] = useState(false);
  const [rawText, setRawText] = useState(() => JSON.stringify(section.value, null, 2));
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setDraft(section.value);
    setRawText(JSON.stringify(section.value, null, 2));
    setError('');
  }, [section.value]);

  // "前往配置" on the jobs page links here as /settings#<section>; open and
  // reveal the matching editor instead of leaving the user to hunt for it.
  const { hash } = useLocation();
  const detailsRef = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    if (hash === `#${section.section}` && detailsRef.current) {
      detailsRef.current.open = true;
      detailsRef.current.scrollIntoView({ block: 'start', behavior: 'smooth' });
    }
  }, [hash, section.section]);

  const Form = SECTION_FORMS[section.section];
  const useRaw = rawMode || Form === undefined;

  // In raw mode the textarea is the source of truth; parse it before validating.
  let parsed: Record<string, unknown> | null = draft;
  let parseError = '';
  if (useRaw) {
    parsed = null;
    try {
      const candidate: unknown = JSON.parse(rawText);
      if (candidate === null || typeof candidate !== 'object' || Array.isArray(candidate)) {
        parseError = '内容必须是一个 JSON 对象';
      } else {
        parsed = candidate as Record<string, unknown>;
      }
    } catch (err) {
      parseError = err instanceof Error ? err.message : 'JSON 解析失败';
    }
  }

  const issues = parsed ? validateSection(section.section, parsed) : [];
  const dirty = JSON.stringify(parsed) !== JSON.stringify(section.value);

  const save = useMutation({
    mutationFn: () => api.put<SettingsSection>(`/api/v2/settings/${section.section}`, parsed),
    onSuccess: () => {
      setError('');
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2500);
      void queryClient.invalidateQueries({ queryKey: ['settings'] });
      void queryClient.invalidateQueries({ queryKey: ['jobs'] });
    },
    onError: (err: Error) => {
      if (err instanceof ApiError && err.details) {
        setError(`${err.message}\n${JSON.stringify(err.details, null, 2)}`);
        return;
      }
      setError(err.message);
    },
  });

  const reset = () => {
    setDraft(section.value);
    setRawText(JSON.stringify(section.value, null, 2));
    setError('');
  };

  return (
    <details ref={detailsRef} id={section.section} className="section-editor">
      <summary>
        <span>{sectionLabel(section.section)}</span>
        <code className="muted">{section.section}</code>
        {dirty && <span className="badge-dirty">未保存</span>}
        {section.missing_fields.length > 0 && <span className="warn">缺少 {section.missing_fields.join(', ')}</span>}
      </summary>

      {useRaw ? (
        <>
          {Form === undefined && <p className="muted">这个 section 没有对应的表单，使用 JSON 编辑。</p>}
          <textarea
            className="json-editor"
            spellCheck={false}
            rows={Math.min(28, rawText.split('\n').length + 2)}
            value={rawText}
            onChange={(event) => setRawText(event.target.value)}
          />
        </>
      ) : (
        <Form
          value={draft}
          onChange={(next) => {
            setDraft(next);
            setRawText(JSON.stringify(next, null, 2));
          }}
        />
      )}

      {section.section === 'notifications.telegram' && <TelegramNotificationTest dirty={dirty} />}

      {/* Authors and seeds live in their own tables and apply immediately, so they
          render outside the form draft and are untouched by 保存/重置. */}
      {section.section === 'web.hanime1' && (
        <>
          <Hanime1Authors />
          <Hanime1Seeds />
        </>
      )}

      {parseError && <p className="warn">{parseError}</p>}
      {issues.length > 0 && (
        <ul className="issues">
          {issues.map((issue) => (
            <li key={issue} className="warn">
              {issue}
            </li>
          ))}
        </ul>
      )}
      {error && <pre className="warn wrap">{error}</pre>}

      <div className="actions">
        <button
          type="button"
          disabled={!parsed || issues.length > 0 || save.isPending}
          onClick={() => save.mutate()}
        >
          {save.isPending ? '保存中…' : '保存'}
        </button>
        <button type="button" className="ghost" onClick={reset}>
          重置
        </button>
        {Form !== undefined && (
          <button
            type="button"
            className="ghost"
            onClick={() => {
              // Carry the current draft across so switching modes never loses edits.
              if (!rawMode) {
                setRawText(JSON.stringify(draft, null, 2));
              } else if (parsed) {
                setDraft(parsed);
              }
              setRawMode(!rawMode);
            }}
            disabled={rawMode && !parsed}
            title={rawMode && !parsed ? 'JSON 无法解析，修好后才能切回表单' : undefined}
          >
            {rawMode ? '返回表单' : 'JSON 编辑'}
          </button>
        )}
        {saved && <span className="ok">已保存</span>}
      </div>
    </details>
  );
}

function Hanime1Seeds() {
  const queryClient = useQueryClient();
  const [seed, setSeed] = useState('');
  const [error, setError] = useState('');

  const seeds = useQuery({
    queryKey: ['hanime1-seeds'],
    queryFn: () => api.get<ListResponse<Hanime1Seed>>('/api/v2/hanime1/seeds'),
  });

  const add = useMutation({
    mutationFn: () => api.post('/api/v2/hanime1/seeds', { seed }),
    onSuccess: () => {
      setSeed('');
      setError('');
      void queryClient.invalidateQueries({ queryKey: ['hanime1-seeds'] });
    },
    onError: (err: Error) => setError(err.message),
  });

  const remove = useMutation({
    mutationFn: (videoId: string) => api.delete(`/api/v2/hanime1/seeds/${videoId}`),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['hanime1-seeds'] }),
    onError: (err: Error) => setError(err.message),
  });

  return (
    <div className="subsection">
      <h4>系列种子</h4>
      <p className="muted">可填写视频 ID、或 {'{id-12345}'} 形式的种子；标题会自动解析，添加后立即生效。</p>

      <div className="inline-form">
        <input
          type="text"
          value={seed}
          placeholder="12345 或 标题 {id-12345}"
          onChange={(event) => setSeed(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && seed.trim()) add.mutate();
          }}
        />
        <button type="button" disabled={!seed.trim() || add.isPending} onClick={() => add.mutate()}>
          {add.isPending ? '解析中…' : '添加'}
        </button>
      </div>
      {error && <p className="warn">{error}</p>}

      {seeds.data?.items.length === 0 && <p className="muted">还没有种子。</p>}
      {seeds.data && seeds.data.items.length > 0 && (
        <table className="table">
          <thead>
            <tr>
              <th>标题</th>
              <th>ID</th>
              <th>已发现</th>
              <th>最近扫描</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {seeds.data.items.map((item) => (
              <tr key={item.video_id}>
                <td>
                  <a href={item.watch_url} target="_blank" rel="noreferrer">
                    {item.title}
                  </a>
                  {item.last_scan_error && <div className="warn">{item.last_scan_error}</div>}
                </td>
                <td className="mono">{item.video_id}</td>
                <td>{item.video_count}</td>
                <td className="muted nowrap">{formatDateTime(item.last_scanned_at)}</td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() => {
                      if (window.confirm(`删除种子「${item.title}」？该系列已发现的视频记录也会一并移除。`)) {
                        remove.mutate(item.video_id);
                      }
                    }}
                  >
                    删除
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function Hanime1Authors() {
  const queryClient = useQueryClient();
  const [author, setAuthor] = useState('');
  const [error, setError] = useState('');

  const authors = useQuery({
    queryKey: ['hanime1-authors'],
    queryFn: () => api.get<ListResponse<Hanime1Author>>('/api/v2/hanime1/authors'),
  });

  const add = useMutation({
    mutationFn: () => api.post('/api/v2/hanime1/authors', { author }),
    onSuccess: () => {
      setAuthor('');
      setError('');
      void queryClient.invalidateQueries({ queryKey: ['hanime1-authors'] });
    },
    onError: (err: Error) => setError(err.message),
  });

  const remove = useMutation({
    mutationFn: (authorId: string) => api.delete(`/api/v2/hanime1/authors/${authorId}`),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['hanime1-authors'] }),
    onError: (err: Error) => setError(err.message),
  });

  return (
    <div className="subsection">
      <h4>作者订阅</h4>
      <p className="muted">可填写作者 ID 或作者页 URL；显示名会自动解析，添加后立即生效，新视频按「分类/作者名」归档。</p>

      <div className="inline-form">
        <input
          type="text"
          value={author}
          placeholder="202534 或 https://hanime1.me/user/202534/uploaded"
          onChange={(event) => setAuthor(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && author.trim()) add.mutate();
          }}
        />
        <button type="button" disabled={!author.trim() || add.isPending} onClick={() => add.mutate()}>
          {add.isPending ? '解析中…' : '添加'}
        </button>
      </div>
      {error && <p className="warn">{error}</p>}

      {authors.data?.items.length === 0 && <p className="muted">还没有作者订阅。</p>}
      {authors.data && authors.data.items.length > 0 && (
        <table className="table">
          <thead>
            <tr>
              <th>作者</th>
              <th>ID</th>
              <th>视频数</th>
              <th>最近扫描</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {authors.data.items.map((item) => (
              <tr key={item.author_id}>
                <td>
                  <a href={item.author_url} target="_blank" rel="noreferrer">
                    {item.name}
                  </a>
                  {item.last_scan_error && <div className="warn">{item.last_scan_error}</div>}
                </td>
                <td className="mono">{item.author_id}</td>
                <td>{item.video_count}</td>
                <td className="muted nowrap">{formatDateTime(item.last_scanned_at)}</td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() => {
                      if (window.confirm(`删除作者订阅「${item.name}」？已下载的视频记录不会被删除。`)) {
                        remove.mutate(item.author_id);
                      }
                    }}
                  >
                    删除
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

export function SettingsPage() {
  const settings = useQuery({
    queryKey: ['settings'],
    queryFn: () => api.get<ListResponse<SettingsSection>>('/api/v2/settings'),
  });

  return (
    <div className="stack">
      <section className="card">
        <h2>配置</h2>
        <p className="muted">
          所有配置存放在数据库里，保存后立即生效。cron 与启用开关在「任务」页调整。
          Telegram 的实时监听在进程启动时建立，改动其账号后需重启 worker。
        </p>
        {settings.isLoading && <p>加载中…</p>}
        {settings.error && <p className="warn">{(settings.error as Error).message}</p>}
        {settings.data?.items
          .filter((section) => !JOBS_PAGE_ONLY_SECTIONS.has(section.section))
          .map((section) => (
            <SectionEditor key={section.section} section={section} />
          ))}
      </section>
    </div>
  );
}
