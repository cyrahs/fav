import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { api } from '../api/client';
import type { Job, JobRequest, ListResponse, SettingsSection } from '../api/types';
import { CronInput, describeCron } from '../components/CronInput';
import { sectionLabel } from '../labels';

function StatusPill({ status }: { status: JobRequest['status'] }) {
  return <span className={`pill pill-${status}`}>{status}</span>;
}

function JobRow({ job }: { job: Job }) {
  const queryClient = useQueryClient();
  const [cron, setCron] = useState(job.cron);
  const [enabled, setEnabled] = useState(job.enabled);
  const [notify, setNotify] = useState(job.notify);
  const [error, setError] = useState('');

  // Re-sync when the list refetches (e.g. after another edit lands).
  useEffect(() => {
    setCron(job.cron);
    setEnabled(job.enabled);
    setNotify(job.notify);
  }, [job.cron, job.enabled, job.notify]);

  const dirty = cron !== job.cron || enabled !== job.enabled || notify !== job.notify;
  const cronValid = describeCron(cron).valid;
  // The API rejects enabling an incomplete source, so block it here rather than
  // letting the user submit a toggle that can only come back as a 422.
  const incomplete = job.missing_fields.length > 0;

  const save = useMutation({
    mutationFn: async () => {
      // Settings are stored per section, so merge into the current value rather
      // than sending a partial document.
      const current = await api.get<SettingsSection>(`/api/v2/settings/${job.section}`);
      return api.put<SettingsSection>(`/api/v2/settings/${job.section}`, {
        ...current.value,
        cron,
        enabled,
        notify,
      });
    },
    onSuccess: () => {
      setError('');
      void queryClient.invalidateQueries({ queryKey: ['jobs'] });
    },
    onError: (err: Error) => setError(err.message),
  });

  const trigger = useMutation({
    mutationFn: () => api.post<JobRequest>('/api/v2/job-requests', { target: job.key }),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['job-requests'] }),
    onError: (err: Error) => setError(err.message),
  });

  return (
    <tr>
      <td>
        <strong>{sectionLabel(job.section, job.name)}</strong>
        <div className="muted mono">{job.key}</div>
        {incomplete && (
          <div className="warn">
            配置不完整，缺少：{job.missing_fields.join(', ')}
            <br />
            <Link to="/settings">前往配置</Link>
          </div>
        )}
        {error && <div className="warn">{error}</div>}
      </td>
      <td className="cron-cell">
        <CronInput id={`cron-${job.key}`} value={cron} onChange={setCron} />
      </td>
      <td>
        <div className="switch-stack">
          <label className="switch" title={incomplete ? '配置不完整，无法启用' : undefined}>
            <input
              type="checkbox"
              checked={enabled}
              disabled={incomplete}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            {/* The checkbox already says on or off; a label that flips with it only
                makes the row harder to read. */}
            <span>{incomplete ? '未就绪' : '启用'}</span>
          </label>
          <label className="switch" title="关闭后这个任务不再发送任何 Telegram 通知，包括运行失败">
            <input type="checkbox" checked={notify} onChange={(e) => setNotify(e.target.checked)} />
            <span>通知</span>
          </label>
        </div>
      </td>
      <td className="actions">
        <button type="button" disabled={!dirty || !cronValid || save.isPending} onClick={() => save.mutate()}>
          {save.isPending ? '保存中…' : '保存'}
        </button>
        <button type="button" className="ghost" disabled={trigger.isPending} onClick={() => trigger.mutate()}>
          立即运行
        </button>
      </td>
    </tr>
  );
}

export function JobsPage() {
  const jobs = useQuery({
    queryKey: ['jobs'],
    queryFn: () => api.get<ListResponse<Job>>('/api/v2/jobs'),
  });

  const requests = useQuery({
    queryKey: ['job-requests'],
    queryFn: () => api.get<ListResponse<JobRequest>>('/api/v2/job-requests?limit=20'),
    // Requests are executed by the worker, so poll while any are in flight.
    refetchInterval: (query) => {
      const items = query.state.data?.items ?? [];
      return items.some((item) => item.status === 'pending' || item.status === 'running') ? 2000 : 15000;
    },
  });

  return (
    <div className="stack">
      <section className="card">
        <h2>调度任务</h2>
        <p className="muted">修改 cron 或开关后保存，worker 会在 15 秒内自动重排，无需重启。</p>
        {jobs.isLoading && <p>加载中…</p>}
        {jobs.error && <p className="warn">{(jobs.error as Error).message}</p>}
        {jobs.data && (
          <table className="table">
            <thead>
              <tr>
                <th>任务</th>
                <th>cron</th>
                <th>状态</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {jobs.data.items.map((job) => (
                <JobRow key={job.key} job={job} />
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="card">
        <h2>运行记录</h2>
        {requests.data?.items.length === 0 && <p className="muted">暂无手动触发记录。</p>}
        {requests.data && requests.data.items.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>#</th>
                <th>目标</th>
                <th>状态</th>
                <th>请求时间</th>
                <th>结束时间</th>
                <th>结果</th>
              </tr>
            </thead>
            <tbody>
              {requests.data.items.map((request) => (
                <tr key={request.id}>
                  <td className="mono">{request.id}</td>
                  <td className="mono">{request.target}</td>
                  <td>
                    <StatusPill status={request.status} />
                  </td>
                  <td className="muted">{request.requested_at}</td>
                  <td className="muted">{request.finished_at ?? '—'}</td>
                  <td className="wrap">{request.error || request.result || '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}
