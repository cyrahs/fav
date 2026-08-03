import { useMutation } from '@tanstack/react-query';
import { api, ApiError } from '../api/client';
import type { TelegramNotificationTest as TestResult } from '../api/types';

const CODE_LABELS: Record<string, string> = {
  telegram_not_configured: '还没保存 bot_token / chat_id',
  telegram_delivery_failed: '发送失败',
};

/**
 * Sends a real message through the stored bot credentials. Unlike the CookieCloud
 * probe this cannot run against an unsaved draft — the backend reads the saved
 * section — so the button tells the user to save first when the section is dirty.
 */
export function TelegramNotificationTest({ dirty }: { dirty: boolean }) {
  const test = useMutation({
    mutationFn: () => api.post<TestResult>('/api/v2/notifications/telegram/test', {}),
  });

  const result = test.data;
  const error = test.error as Error | undefined;

  return (
    <div className="cc-test">
      <button
        type="button"
        className="ghost"
        disabled={test.isPending || dirty}
        title={dirty ? '先保存，测试用的是已保存的凭据' : undefined}
        onClick={() => test.mutate()}
      >
        {test.isPending ? '发送中…' : '发送测试消息'}
      </button>

      {dirty && <span className="muted">有未保存的改动，测试用的是已保存的凭据</span>}

      {error && (
        <span className="warn">
          ✗ {error instanceof ApiError ? (CODE_LABELS[error.code] ?? error.code) : '出错了'} — {error.message}
        </span>
      )}

      {result && !error && (
        <span className="ok">
          ✓ 已发送{result.message_id !== null && `（message ${result.message_id}）`}
          {result.warnings.length > 0 && ` — ${result.warnings.join('；')}`}
        </span>
      )}
    </div>
  );
}
