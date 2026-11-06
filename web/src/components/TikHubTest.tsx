import { useMutation } from '@tanstack/react-query';
import { api, ApiError } from '../api/client';
import type { TikHubTest as TestResult } from '../api/types';

const CODE_LABELS: Record<string, string> = {
  tikhub_not_configured: '还没保存 API Key',
  tikhub_unreachable: '连不上 TikHub',
  tikhub_key_refused: 'TikHub 拒绝了这个 key',
};

function dollars(amount: number): string {
  return `$${amount.toFixed(2)}`;
}

/**
 * Looks up the saved key's account on TikHub (a free call). Like the other tests it
 * reads the saved section, so it waits for unsaved edits to be saved first.
 */
export function TikHubTest({ dirty }: { dirty: boolean }) {
  const test = useMutation({
    mutationFn: () => api.post<TestResult>('/api/v2/services/tikhub/test', {}),
  });

  const result = test.data;
  const error = test.error as Error | undefined;

  return (
    <div className="cc-test">
      <button
        type="button"
        className="ghost"
        disabled={test.isPending || dirty}
        title={dirty ? '先保存，检查用的是已保存的 key' : undefined}
        onClick={() => test.mutate()}
      >
        {test.isPending ? '检查中…' : '检查 Key 和余额'}
      </button>

      {dirty && <span className="muted">有未保存的改动，检查用的是已保存的 key</span>}

      {error && (
        <span className="warn">
          ✗ {error instanceof ApiError ? (CODE_LABELS[error.code] ?? error.code) : '出错了'} — {error.message}
        </span>
      )}

      {result && !error && (
        <span className="ok">
          ✓ 可用{result.key_name && `（${result.key_name}）`}，余额 {dollars(result.balance)}，赠送额度{' '}
          {dollars(result.free_credit)}
        </span>
      )}
    </div>
  );
}
