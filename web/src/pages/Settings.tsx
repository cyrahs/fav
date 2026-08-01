import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { api, ApiError } from '../api/client';
import type { Hanime1Seed, ListResponse, SettingsSection } from '../api/types';
import { describeCron } from '../components/CronInput';

const SECTION_LABELS: Record<string, string> = {
  'web.bilibili': 'Bilibili',
  'web.telegram': 'Telegram',
  'web.stellasora': 'StellaSora',
  'web.nikke': 'NIKKE',
  'web.bd2': 'BD2',
  'web.azurlane': '碧蓝航线',
  'web.hanime1': 'Hanime1',
  'web.jandan': '煎蛋',
  'web.kemono': 'Kemono',
  cookiecloud: 'CookieCloud',
  nasuchan: 'Nasuchan 通知',
};

const SECRET_HINT = '留空或保持掩码不变即不修改。';

function hasSecret(section: string): boolean {
  return section === 'cookiecloud' || section === 'nasuchan' || section === 'web.telegram';
}

/**
 * Sections vary in shape (nested accounts, creator lists, ranking config), so the
 * editor is a JSON textarea with validation rather than a bespoke form per source.
 * The cron field gets a live natural-language hint since it is the value most
 * often edited by hand.
 */
function SectionEditor({ section }: { section: SettingsSection }) {
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState(() => JSON.stringify(section.value, null, 2));
  const [error, setError] = useState('');
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setDraft(JSON.stringify(section.value, null, 2));
    setError('');
  }, [section.value]);

  let parsed: Record<string, unknown> | null = null;
  let parseError = '';
  try {
    const candidate = JSON.parse(draft);
    if (candidate === null || typeof candidate !== 'object' || Array.isArray(candidate)) {
      parseError = '内容必须是一个 JSON 对象';
    } else {
      parsed = candidate as Record<string, unknown>;
    }
  } catch (err) {
    parseError = err instanceof Error ? err.message : 'JSON 解析失败';
  }

  const cron = typeof parsed?.cron === 'string' ? parsed.cron : '';
  const cronDescription = cron ? describeCron(cron) : null;

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

  return (
    <details className="section-editor">
      <summary>
        <span>{SECTION_LABELS[section.section] ?? section.section}</span>
        <code className="muted">{section.section}</code>
        {section.missing_fields.length > 0 && <span className="warn">缺少 {section.missing_fields.join(', ')}</span>}
      </summary>

      {hasSecret(section.section) && <p className="muted">{SECRET_HINT}</p>}

      <textarea
        className="json-editor"
        spellCheck={false}
        rows={Math.min(28, draft.split('\n').length + 2)}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
      />

      {cronDescription && (
        <p className={cronDescription.valid ? 'cron-hint' : 'cron-hint cron-hint-error'}>
          cron：{cronDescription.text}
        </p>
      )}
      {parseError && <p className="warn">{parseError}</p>}
      {error && <pre className="warn wrap">{error}</pre>}

      <div className="actions">
        <button type="button" disabled={!parsed || save.isPending} onClick={() => save.mutate()}>
          {save.isPending ? '保存中…' : '保存'}
        </button>
        <button type="button" className="ghost" onClick={() => setDraft(JSON.stringify(section.value, null, 2))}>
          重置
        </button>
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
    <section className="card">
      <h2>Hanime1 系列种子</h2>
      <p className="muted">可填写视频 ID、或 {'{id-12345}'} 形式的种子；标题会自动解析。</p>

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
                <td className="muted">{item.last_scanned_at ?? '—'}</td>
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
    </section>
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
          所有配置存放在数据库里，保存后立即生效（调度变更最多延迟 15 秒）。
          Telegram 的实时监听在进程启动时建立，改动其账号后需重启 worker。
        </p>
        {settings.isLoading && <p>加载中…</p>}
        {settings.error && <p className="warn">{(settings.error as Error).message}</p>}
        {settings.data?.items.map((section) => (
          <SectionEditor key={section.section} section={section} />
        ))}
      </section>

      <Hanime1Seeds />
    </div>
  );
}
