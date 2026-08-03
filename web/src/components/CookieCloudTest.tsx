import { useMutation } from '@tanstack/react-query';
import { api } from '../api/client';
import type { CookieCloudTestResult } from '../api/types';

interface CookieCloudTestProps {
  /** Names the account a masked password resolves against. */
  account: string;
  serverUrl: string;
  uuid: string;
  password: string;
}

const CODE_LABELS: Record<string, string> = {
  incomplete: '配置不完整',
  unreachable: '连不上服务器',
  http_error: '服务器返回错误',
  decrypt_failed: 'UUID 或密码不对',
  no_domain_cookies: '缺少 bilibili.com 的 cookie',
  missing_cookies: 'cookie 不齐全',
  error: '出错了',
};

/**
 * Tests the credentials as currently typed, without saving them. The draft is sent
 * as-is; the backend swaps a masked password for the one stored under this account.
 */
export function CookieCloudTest({ account, serverUrl, uuid, password }: CookieCloudTestProps) {
  const test = useMutation({
    mutationFn: () =>
      api.post<CookieCloudTestResult>('/api/v2/cookiecloud/test', {
        account,
        server_url: serverUrl,
        uuid,
        password,
      }),
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
          {result.ok ? '✓ ' : '✗ '}
          {result.ok ? '可用' : (CODE_LABELS[result.code] ?? result.code)}
          {' — '}
          {result.message}
        </span>
      )}
    </div>
  );
}
