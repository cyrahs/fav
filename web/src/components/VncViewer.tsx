import { useEffect, useRef, useState } from 'react';
import RFB from '@novnc/novnc';

type Status = 'connecting' | 'connected' | 'disconnected';

/**
 * The login browser's screen, over the API's VNC WebSocket. The ticket is the
 * login's own key (the socket cannot carry the bearer token), so a stale page
 * from an earlier login simply fails to connect.
 */
export function VncViewer({ ticket }: { ticket: string }) {
  const screenRef = useRef<HTMLDivElement>(null);
  const rfbRef = useRef<RFB | null>(null);
  const [status, setStatus] = useState<Status>('connecting');
  const [text, setText] = useState('');

  useEffect(() => {
    const target = screenRef.current;
    if (!target) return undefined;
    const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${scheme}://${window.location.host}/api/v2/site-sessions/vnc?ticket=${encodeURIComponent(ticket)}`;
    const rfb = new RFB(target, url, { wsProtocols: ['binary'] });
    rfb.scaleViewport = true;
    rfb.focusOnClick = true;
    rfb.qualityLevel = 8;
    const onConnect = () => setStatus('connected');
    const onDisconnect = () => setStatus('disconnected');
    rfb.addEventListener('connect', onConnect);
    rfb.addEventListener('disconnect', onDisconnect);
    rfbRef.current = rfb;
    setStatus('connecting');
    return () => {
      rfb.removeEventListener('connect', onConnect);
      rfb.removeEventListener('disconnect', onDisconnect);
      rfb.disconnect();
      rfbRef.current = null;
    };
  }, [ticket]);

  // Typing into the remote window works directly; this is for passwords kept in
  // a local password manager, which cannot autofill through a canvas. The text
  // is typed as key events into whatever field has focus remotely, which needs
  // no clipboard support on either side.
  const sendText = () => {
    const rfb = rfbRef.current;
    if (!rfb || !text) return;
    for (const char of text) {
      const code = char.codePointAt(0) ?? 0;
      // X keysyms: Latin-1 maps to itself, everything else to 0x01000000 + code point.
      const keysym = code < 0x100 ? code : 0x01000000 + code;
      rfb.sendKey(keysym, null, true);
      rfb.sendKey(keysym, null, false);
    }
    rfb.focus();
    setText('');
  };

  return (
    <div className="vnc">
      <div className="actions">
        <span className={status === 'connected' ? 'ok' : 'muted'}>
          {status === 'connected' ? '已连接' : status === 'connecting' ? '连接中…' : '已断开'}
        </span>
        <input
          type="password"
          value={text}
          placeholder="要输入的文本，如密码"
          onChange={(event) => setText(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter') sendText();
          }}
        />
        <button type="button" className="ghost" disabled={!text || status !== 'connected'} onClick={sendText}>
          输入到远程
        </button>
        <span className="muted">先在远程画面里点中输入框，再把文本（可从密码管理器粘贴）送过去。</span>
      </div>
      <div className="vnc-screen" ref={screenRef} />
    </div>
  );
}
