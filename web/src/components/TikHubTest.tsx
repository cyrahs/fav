import { useQuery } from '@tanstack/react-query';
import { api, ApiError } from '../api/client';
import type { TikHubTest as AccountResult } from '../api/types';

/** Shared with the settings page, which refreshes it after a save. */
export const TIKHUB_ACCOUNT_QUERY_KEY = ['tikhub-account'];

const CODE_LABELS: Record<string, string> = {
  tikhub_not_configured: '还没保存 API Key',
  tikhub_unreachable: '连不上 TikHub',
  tikhub_key_refused: 'TikHub 拒绝了这个 key',
};

function dollars(amount: number): string {
  return `$${amount.toFixed(2)}`;
}

/**
 * The saved key's account on TikHub (a free lookup), shown as soon as the section
 * opens, with a button to look again. It reads the saved section, so unsaved edits
 * only show up after a save.
 */
export function TikHubTest({ dirty }: { dirty: boolean }) {
  const account = useQuery({
    queryKey: TIKHUB_ACCOUNT_QUERY_KEY,
    queryFn: () => api.post<AccountResult>('/api/v2/services/tikhub/test', {}),
    retry: false,
    refetchOnWindowFocus: false,
    staleTime: 5 * 60 * 1000,
  });

  const result = account.data;
  const error = account.error as Error | null;

  return (
    <div className="cc-test">
      <button type="button" className="ghost" disabled={account.isFetching} onClick={() => void account.refetch()}>
        {account.isFetching ? '查询中…' : '刷新'}
      </button>

      {account.isPending && <span className="muted">正在查询 key 和余额…</span>}

      {error && !account.isFetching && (
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

      {dirty && <span className="muted">显示的是已保存的 key，保存后会自动刷新</span>}
    </div>
  );
}
