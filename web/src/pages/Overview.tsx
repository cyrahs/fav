import { useQuery } from '@tanstack/react-query';
import { api, getReadiness } from '../api/client';
import type { ArchiveSourceStat, Health, Job, JobRequest, ListResponse } from '../api/types';
import { describeCron } from '../components/CronInput';
import { StatusPill } from '../components/StatusPill';
import { formatDateTime } from '../format';
import { kindLabel, sectionLabel, sourceLabel, targetLabel } from '../labels';

export function OverviewPage() {
  const health = useQuery({
    queryKey: ['health'],
    queryFn: () => api.get<Health>('/healthz'),
    refetchInterval: 30000,
  });

  const readiness = useQuery({
    // getReadiness resolves the degraded 503 as data, so the per-check messages
    // below stay available exactly when they matter.
    queryKey: ['readiness'],
    queryFn: getReadiness,
    retry: false,
    refetchInterval: 60000,
  });

  const jobs = useQuery({ queryKey: ['jobs'], queryFn: () => api.get<ListResponse<Job>>('/api/v2/jobs') });
  const sources = useQuery({
    queryKey: ['archive-sources'],
    queryFn: () => api.get<ListResponse<ArchiveSourceStat>>('/api/v2/archive/sources'),
  });
  const requests = useQuery({
    // Distinct from the Jobs page key: same endpoint, different limit/filter.
    queryKey: ['job-requests', 'overview'],
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
            {/* Neutral placeholder while loading, so the page never flashes a false alarm. */}
            {health.isLoading ? (
              <span className="muted">检查中…</span>
            ) : (
              <span className={health.data ? 'ok' : 'warn'}>{health.data ? '运行中' : '不可达'}</span>
            )}
          </div>
          <div className="stat">
            <span className="stat-label">就绪检查</span>
            {readiness.isLoading ? (
              <span className="muted">检查中…</span>
            ) : readiness.data ? (
              <span className={readiness.data.status === 'ok' ? 'ok' : 'warn'}>
                {readiness.data.status === 'ok' ? '正常' : '降级'}
              </span>
            ) : (
              <span className="warn">不可达</span>
            )}
          </div>
          <div className="stat">
            <span className="stat-label">启用任务</span>
            <span>
              {enabledJobs.length} / {jobs.data?.items.length ?? 0}
            </span>
          </div>
        </div>
        {readiness.data?.checks &&
          Object.entries(readiness.data.checks)
            .filter(([, check]) => check.status !== 'ok')
            .map(([name, check]) => (
              <p key={name} className="warn">
                {name}: {check.message}
              </p>
            ))}
        {incomplete.length > 0 && (
          <p className="warn">
            配置不完整：
            {incomplete
              .map((job) => `${sectionLabel(job.section, job.name)} (${job.missing_fields.join(', ')})`)
              .join('；')}
          </p>
        )}
      </section>

      <section className="card">
        <h2>已归档</h2>
        <div className="stat-row">
          {sources.data?.items.map((item) => (
            <div className="stat" key={item.source}>
              <span className="stat-label">{sourceLabel(item.source, item.name)}</span>
              <span className="stat-value">{item.total}</span>
              <span className="muted">{formatDateTime(item.latest_at)}</span>
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
              <strong>{sectionLabel(job.section, job.name)}</strong>
              <code className="mono">{job.cron}</code>
              <span className="muted">{describeCron(job.cron).text}</span>
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>最近运行</h2>
        {requests.data?.items.length === 0 && <p className="muted">暂无记录。</p>}
        <ul className="plain-list">
          {requests.data?.items.map((request) => (
            <li key={request.id}>
              <StatusPill status={request.status} />
              <strong>{targetLabel(request.target)}</strong>
              <span className="muted">{kindLabel(request.kind)}</span>
              <span className="muted">{formatDateTime(request.requested_at)}</span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}
