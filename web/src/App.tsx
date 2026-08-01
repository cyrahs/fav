import { useEffect, useState } from 'react';
import { NavLink, Navigate, Route, Routes } from 'react-router-dom';
import { clearToken, getToken, onUnauthorized, setToken, verifyToken } from './api/client';
import { JobsPage } from './pages/Jobs';
import { OverviewPage } from './pages/Overview';
import { RecordsPage } from './pages/Records';
import { SettingsPage } from './pages/Settings';

function LoginScreen({ onAuthenticated }: { onAuthenticated: () => void }) {
  const [token, setTokenValue] = useState('');
  const [error, setError] = useState('');
  const [checking, setChecking] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setChecking(true);
    setError('');
    try {
      if (await verifyToken(token.trim())) {
        setToken(token.trim());
        onAuthenticated();
      } else {
        setError('令牌无效。');
      }
    } catch {
      setError('无法连接到 API。');
    } finally {
      setChecking(false);
    }
  };

  return (
    <div className="login">
      <form className="card" onSubmit={submit}>
        <h1>fav 管理</h1>
        <p className="muted">输入部署的 API_TOKEN。它只保存在本标签页的 sessionStorage 中。</p>
        <input
          type="password"
          value={token}
          autoFocus
          placeholder="API_TOKEN"
          onChange={(event) => setTokenValue(event.target.value)}
        />
        {error && <p className="warn">{error}</p>}
        <button type="submit" disabled={!token.trim() || checking}>
          {checking ? '验证中…' : '登录'}
        </button>
      </form>
    </div>
  );
}

export function App() {
  const [authenticated, setAuthenticated] = useState(() => Boolean(getToken()));

  useEffect(() => onUnauthorized(() => setAuthenticated(false)), []);

  if (!authenticated) {
    return <LoginScreen onAuthenticated={() => setAuthenticated(true)} />;
  }

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">fav</span>
        <nav>
          <NavLink to="/" end>
            概览
          </NavLink>
          <NavLink to="/records">记录</NavLink>
          <NavLink to="/jobs">任务</NavLink>
          <NavLink to="/settings">配置</NavLink>
        </nav>
        <button
          type="button"
          className="ghost"
          onClick={() => {
            clearToken();
            setAuthenticated(false);
          }}
        >
          退出
        </button>
      </header>

      <main>
        <Routes>
          <Route path="/" element={<OverviewPage />} />
          <Route path="/records" element={<RecordsPage />} />
          <Route path="/jobs" element={<JobsPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
    </div>
  );
}
