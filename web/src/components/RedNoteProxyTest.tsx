import { useMutation } from '@tanstack/react-query';
import { api } from '../api/client';
import type { RedNoteProxyTestResult } from '../api/types';

interface RedNoteProxyTestProps {
  proxy: string;
}

const CODE_LABELS: Record<string, string> = {
  invalid: '代理写法不对',
  proxy_error: '代理本身不通',
  unreachable: '代理通了但连不上小红书',
  risk_control: '这个出口被风控挡了',
  http_error: '站点返回错误',
};

/**
 * Tests the egress as currently typed, without saving it. The draft is sent as-is;
 * the backend swaps a masked value for the one already stored.
 *
 * The exit address is the point of the readout, not decoration: a datacenter range
 * is what got the account signed out everywhere, and it is the one thing here that
 * an operator can judge at a glance.
 */
export function RedNoteProxyTest({ proxy }: RedNoteProxyTestProps) {
  const test = useMutation({
    mutationFn: () => api.post<RedNoteProxyTestResult>('/api/v2/rednote/proxy/test', { proxy }),
  });

  const result = test.data;

  return (
    <div className="cc-test">
      <button type="button" className="ghost" disabled={test.isPending} onClick={() => test.mutate()}>
        {test.isPending ? '测试中…' : '测试出口'}
      </button>

      {test.error && <span className="warn">{(test.error as Error).message}</span>}

      {result && (
        <span className={result.ok ? 'ok' : 'warn'}>
          {result.ok ? '✓ 可用' : `✗ ${CODE_LABELS[result.code] ?? result.code}`}
          {result.direct && '（直连，未走代理）'}
          {result.exit_ip && ` — 出口 ${result.exit_ip}`}
          {!result.ok && ` — ${result.message}`}
        </span>
      )}
    </div>
  );
}
