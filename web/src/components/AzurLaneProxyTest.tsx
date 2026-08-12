import { useMutation } from '@tanstack/react-query';
import { api } from '../api/client';
import type { AzurLaneProxyTestResult } from '../api/types';

interface AzurLaneProxyTestProps {
  originProxy: string;
}

const CODE_LABELS: Record<string, string> = {
  incomplete: '还没填代理',
  proxy_error: '代理拒绝连接',
  unreachable: '连不上 l2d.su',
  http_error: '源站返回错误',
};

/**
 * Tests the proxy as currently typed, without saving it. The draft is sent as-is;
 * the backend swaps a masked value for the one already stored.
 */
export function AzurLaneProxyTest({ originProxy }: AzurLaneProxyTestProps) {
  const test = useMutation({
    mutationFn: () => api.post<AzurLaneProxyTestResult>('/api/v2/azurlane/proxy/test', { origin_proxy: originProxy }),
  });

  const result = test.data;

  return (
    <div className="cc-test">
      <button type="button" className="ghost" disabled={test.isPending} onClick={() => test.mutate()}>
        {test.isPending ? '测试中…' : '测试连接'}
      </button>

      {test.error && <span className="warn">{(test.error as Error).message}</span>}

      {result && (
        <span className={result.ok ? 'ok' : 'warn'}>
          {result.ok ? '✓ 可用' : `✗ ${CODE_LABELS[result.code] ?? result.code}`}
          {result.exit_ip && ` — 出口 ${result.exit_ip}`}
          {!result.ok && ` — ${result.message}`}
        </span>
      )}
    </div>
  );
}
