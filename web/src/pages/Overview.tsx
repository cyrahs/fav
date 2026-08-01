import { useQuery } from '@tanstack/react-query';
import { api } from '../api/client';
import type { ArchiveSourceStat, Health, Job, JobRequest, ListResponse, Readiness } from '../api/types';
import { describeCron } from '../components/CronInput';

export function OverviewPage() {
  const health = useQuery({
    queryKey: ['health'],
    queryFn: () => api.get<Health>('/healthz'),
    refetchInterval: 30000,
  });

  const readiness = useQuery({
    // /readyz answers 503 when degraded, which the client surfaces as an error;
    // that is the state worth showing, so keep the error rather than retrying.
    queryKey: ['readiness'],
    queryFn: () => api.get<Readiness>('/readyz'),
    retry: false,
    refetchInterval: 60000,
  });

  const jobs = useQuery({ queryKey: ['jobs'], queryFn: () => api.get<ListResponse<Job>>('/api/v2/jobs') });
  const sources = useQuery({
    queryKey: ['archive-sources'],
    queryFn: () => api.get<ListResponse<ArchiveSourceStat>>('/api/v2/archive/sources'),
  });
  const requests = useQuery({
    queryKey: ['job-requests'],
    queryFn: () => api.get<ListResponse<JobRequest>>('/api/v2/job-requests?limit=5'),
  });

  const enabledJobs = jobs.data?.items.filter((job) => job.enabled) ?? [];
  const incomplete = jobs.data?.items.filter((job) => job.missing_fields.length > 0) ?? [];

  return (
    <div className="stack">
      <section className="card">
        <h2>状态</h2>
        <div className="stat-row">
          <div className="stat">
            <span className="stat-label">进程</span>
            <span className={health.data ? 'ok' : 'warn'}>{health.data ? '运行中' : '不可达'}</span>
          </div>
          <div className="stat">
            <span className="stat-label">就绪检查</span>
            <span className={readiness.data?.status === 'ok' ? 'ok' : 'warn'}>
              {readiness.data?.status ?? 'degraded'}
            </span>
          </div>
          <div className="stat">
            <span className="stat-label">启用任务</span>
            <span>
              {enabledJobs.length} / {jobs.data?.items.length ?? 0}
            </span>
          </div>
        </div>
        {readiness.data?.checks &&
          Object.entries(readiness.data.checks).map(([name, check]) => (
            <p key={name} className="muted">
              {name}: {check.message}
            </p>
          ))}
        {incomplete.length > 0 && (
          <p className="warn">
            配置不完整：{incomplete.map((job) => `${job.name} (${job.missing_fields.join(', ')})`).join('；')}
          </p>
        )}
      </section>

      <section className="card">
        <h2>已归档</h2>
        <div className="stat-row">
          {sources.data?.items.map((item) => (
            <div className="stat" key={item.source}>
              <span className="stat-label">{item.name}</span>
              <span className="stat-value">{item.total}</span>
              <span className="muted">{item.latest_at?.slice(0, 16).replace('T', ' ') ?? '—'}</span>
            </div>
          ))}
        </div>
      </section>

      <section className="card">
        <h2>启用中的调度</h2>
        {enabledJobs.length === 0 && <p className="muted">当前没有启用的任务。</p>}
        <ul className="plain-list">
          {enabledJobs.map((job) => (
            <li key={job.key}>
              <strong>{job.name}</strong>
              <code className="mono">{job.cron}</code>
              <span className="muted">{describeCron(job.cron).text}</span>
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>最近触发</h2>
        {requests.data?.items.length === 0 && <p className="muted">暂无记录。</p>}
        <ul className="plain-list">
          {requests.data?.items.map((request) => (
            <li key={request.id}>
              <span className={`pill pill-${request.status}`}>{request.status}</span>
              <strong>{request.target}</strong>
              <span className="muted">{request.requested_at}</span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}
