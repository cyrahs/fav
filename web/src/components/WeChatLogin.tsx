import { useEffect, useRef, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { api } from '../api/client';
import type { WeChatLoginPoll, WeChatLoginStart } from '../api/types';

interface WeChatLoginProps {
  account: string;
  transport: 'ilink' | 'filehelper';
  path: string;
  mediaTypes: string[];
  /** Called with the stored (token-masked) account once the scan is confirmed. */
  onBound: (account: Record<string, unknown>) => void;
}

type Phase = 'idle' | 'starting' | 'waiting' | 'scanned' | 'confirmed' | 'expired' | 'error';

interface LoginState {
  phase: Phase;
  qrcodeImage?: string;
  message?: string;
}

const ACCOUNT_NAME_RE = /^[A-Za-z0-9_-]+$/;

/**
 * Binds an iLink bot to this account by QR scan. The token never reaches the
 * browser: the backend stores it on confirmation and hands back the masked
 * account, which is patched into the draft so 保存 keeps it.
 */
export function WeChatLogin({ account, transport, path, mediaTypes, onBound }: WeChatLoginProps) {
  const queryClient = useQueryClient();
  const [state, setState] = useState<LoginState>({ phase: 'idle' });
  const cancelled = useRef(false);

  useEffect(
    () => () => {
      cancelled.current = true;
    },
    [],
  );

  const nameOk = ACCOUNT_NAME_RE.test(account);
  const busy = state.phase === 'starting' || state.phase === 'waiting' || state.phase === 'scanned';

  const start = async () => {
    cancelled.current = false;
    setState({ phase: 'starting' });
    try {
      const started = await api.post<WeChatLoginStart>('/api/v2/wechat/login/start', {
        account,
        transport,
        path,
        media_types: mediaTypes,
      });
      setState({ phase: 'waiting', qrcodeImage: started.qrcode_image });
      while (!cancelled.current) {
        const polled = await api.post<WeChatLoginPoll>('/api/v2/wechat/login/poll', { session_key: started.session_key });
        if (cancelled.current) {
          return;
        }
        if (polled.status === 'confirmed') {
          setState({ phase: 'confirmed' });
          if (polled.account) {
            onBound(polled.account);
          }
          void queryClient.invalidateQueries({ queryKey: ['settings'] });
          void queryClient.invalidateQueries({ queryKey: ['jobs'] });
          return;
        }
        if (polled.status === 'expired') {
          setState({ phase: 'expired' });
          return;
        }
        if (polled.status === 'scaned') {
          setState((prev) => ({ ...prev, phase: 'scanned' }));
        }
      }
    } catch (err) {
      if (!cancelled.current) {
        setState({ phase: 'error', message: (err as Error).message });
      }
    }
  };

  const cancel = () => {
    cancelled.current = true;
    setState({ phase: 'idle' });
  };

  return (
    <div className="wechat-login">
      <div className="cc-test">
        <button type="button" className="ghost" disabled={busy || !nameOk} onClick={() => void start()}>
          {state.phase === 'starting' ? '获取二维码…' : busy ? '等待扫码…' : '扫码绑定'}
        </button>
        {busy && (
          <button type="button" className="ghost" onClick={cancel}>
            取消
          </button>
        )}
        {!nameOk && <span className="muted">先填写合法的账号名称</span>}
        {state.phase === 'scanned' && <span className="ok">{transport === 'filehelper' ? '已扫码，请在手机上确认登录' : '已扫码，请在微信里点“连接”'}</span>}
        {state.phase === 'confirmed' && <span className="ok">✓ 已绑定，凭据已保存到服务器</span>}
        {state.phase === 'expired' && <span className="warn">二维码已过期，请重新获取</span>}
        {state.phase === 'error' && <span className="warn">{state.message}</span>}
      </div>
      {busy && state.qrcodeImage && (
        <div className="wechat-qr">
          <img src={state.qrcodeImage} alt="微信扫码二维码" width={220} height={220} />
          <p className="field-hint">
            {transport === 'filehelper'
              ? '用微信“扫一扫”扫描，手机上确认登录网页版文件传输助手后本页会自动完成。手机顶部之后会一直显示“网页版文件传输助手已打开”，那是正常状态。'
              : '用微信扫描（微信内会打开 ClawBot 连接页），点“连接”后本页会自动完成。'}
            绑定会立即保存这个账号的名称、路径和媒体类型。
          </p>
        </div>
      )}
    </div>
  );
}
