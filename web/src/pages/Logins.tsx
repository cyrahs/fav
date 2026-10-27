import { Suspense, lazy } from 'react';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ApiError, api } from '../api/client';
import type { LoginAttempt, SiteSession, SiteSessionList } from '../api/types';
import { formatDateTime } from '../format';

// noVNC is most of this page's weight and only needed while a login is open.
const VncViewer = lazy(() => import('../components/VncViewer').then((module) => ({ default: module.VncViewer })));

const QUERY_KEY = ['site-sessions'];

const LOGIN_STATE_LABELS: Record<LoginAttempt['state'], string> = {
  starting: '正在启动浏览器…',
  waiting: '等待登录',
  saved: '登录成功，会话已保存，浏览器已关闭',
  failed: '浏览器启动失败',
  timeout: '超时未完成登录，浏览器已关闭',
  cancelled: '已取消，浏览器已关闭',
};

function sessionStatus(session: SiteSession): { text: string; className: string } {
  if (!session.signed_in) return { text: '未登录', className: 'warn' };
  if (session.status === 'invalid') return { text: '已失效', className: 'warn' };
  if (session.status === 'valid') return { text: '有效', className: 'ok' };
  return { text: '未验证', className: 'muted' };
}

function errorText(error: unknown): string {
  if (error instanceof ApiError) return error.message;
  return error instanceof Error ? error.message : String(error);
}

export function LoginsPage() {
  const queryClient = useQueryClient();
  const sessions = useQuery({
    queryKey: QUERY_KEY,
    queryFn: () => api.get<SiteSessionList>('/api/v2/site-sessions'),
    // Fast while a login is open, so the page notices the save and drops the
    // remote screen as soon as the browser is closed.
    refetchInterval: (query) => (query.state.data?.login?.active ? 2000 : 30000),
  });

  const refresh = () => queryClient.invalidateQueries({ queryKey: QUERY_KEY });
  const start = useMutation({
    mutationFn: (site: string) => api.post<LoginAttempt>(`/api/v2/site-sessions/${site}/login`, {}),
    onSettled: refresh,
  });
  const cancel = useMutation({
    mutationFn: () => api.delete<LoginAttempt | null>('/api/v2/site-sessions/login'),
    onSettled: refresh,
  });
  const check = useMutation({
    mutationFn: (site: string) => api.post<SiteSession>(`/api/v2/site-sessions/${site}/check`, {}),
    onSettled: refresh,
  });

  const login = sessions.data?.login ?? null;
  const active = login?.active ? login : null;
  const labelFor = (site: string) => sessions.data?.items.find((item) => item.site === site)?.label ?? site;

  return (
    <div className="stack">
      <section className="card">
        <h2>登录态</h2>
        <p className="muted">
          需要登录的来源在这里登录一次即可：点「登录」会临时启动一个浏览器，在下方远程画面里完成登录后，系统自动检测、保存会话并关闭浏览器。平时不常驻任何浏览器。
        </p>
        {sessions.isError && <p className="warn">读取失败：{errorText(sessions.error)}</p>}
        <table className="table">
          <thead>
            <tr>
              <th>站点</th>
              <th>状态</th>
              <th>账号</th>
              <th>登录时间</th>
              <th>Cookie 过期</th>
              <th>上次验证</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {sessions.data?.items.map((session) => {
              const status = sessionStatus(session);
              return (
                <tr key={session.site}>
                  <td>
                    <strong>{session.label}</strong>
                  </td>
                  <td>
                    <span className={status.className}>{status.text}</span>
                    {session.message && <div className="muted wrap">{session.message}</div>}
                  </td>
                  <td className="mono">{session.account || '—'}</td>
                  <td>{formatDateTime(session.signed_in_at)}</td>
                  <td>{session.signed_in ? (session.expires_at ? formatDateTime(session.expires_at) : '浏览器会话') : '—'}</td>
                  <td>{formatDateTime(session.checked_at)}</td>
                  <td className="actions">
                    <button type="button" disabled={Boolean(active) || start.isPending} onClick={() => start.mutate(session.site)}>
                      {session.signed_in ? '重新登录' : '登录'}
                    </button>
                    <button
                      type="button"
                      className="ghost"
                      disabled={!session.signed_in || check.isPending}
                      onClick={() => check.mutate(session.site)}
                    >
                      {check.isPending && check.variables === session.site ? '检测中…' : '检测'}
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        {start.isError && <p className="warn">{errorText(start.error)}</p>}
        {check.isError && <p className="warn">{errorText(check.error)}</p>}
        {login && !login.active && (
          <p className={login.state === 'saved' ? 'ok' : 'warn'}>
            {labelFor(login.site)}：{LOGIN_STATE_LABELS[login.state]}
            {login.state === 'saved' && login.account ? `（账号 ${login.account}）` : ''}
            {login.message && login.state !== 'saved' ? `：${login.message}` : ''}
          </p>
        )}
      </section>

      {active && (
        <section className="card">
          <div className="actions">
            <h2>登录 {labelFor(active.site)}</h2>
            <span className="muted">{LOGIN_STATE_LABELS[active.state]}</span>
            <span className="muted">{formatDateTime(active.deadline)} 前未完成会自动关闭</span>
            <button type="button" className="danger" disabled={cancel.isPending} onClick={() => cancel.mutate()}>
              取消并关闭浏览器
            </button>
          </div>
          {active.message && <p className="muted">检测结果：{active.message}</p>}
          {active.state === 'waiting' && active.ticket ? (
            <Suspense fallback={<p className="muted">加载远程画面…</p>}>
              <VncViewer ticket={active.ticket} />
            </Suspense>
          ) : (
            <p className="muted">浏览器启动中…</p>
          )}
        </section>
      )}
    </div>
  );
}
