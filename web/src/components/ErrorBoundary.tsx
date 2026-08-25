import { Component, type ReactNode } from 'react';

interface ErrorBoundaryState {
  error: Error | null;
}

/** Last-resort guard: a render crash shows a reload path instead of a blank page. */
export class ErrorBoundary extends Component<{ children: ReactNode }, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  render() {
    if (!this.state.error) {
      return this.props.children;
    }
    return (
      <div className="login">
        <div className="card">
          <h1>页面出错了</h1>
          <p className="muted">刷新重试；如果反复出现，请带上下面的错误信息排查。</p>
          <pre className="warn wrap mono">{this.state.error.message}</pre>
          <button type="button" onClick={() => window.location.reload()}>
            刷新页面
          </button>
        </div>
      </div>
    );
  }
}
