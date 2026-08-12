const TOKEN_KEY = 'fav.api.token';
/**
 * How long a login survives without re-entering the token. The window slides on
 * use (see `getToken`), so an active browser stays logged in and one that sat
 * untouched for the whole window has to authenticate again.
 */
export const TOKEN_TTL_DAYS = 30;
const TOKEN_TTL_MS = TOKEN_TTL_DAYS * 24 * 60 * 60 * 1000;

interface StoredToken {
  token: string;
  expiresAt: number;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly details?: unknown,
  ) {
    super(message);
  }
}

function readStoredToken(): StoredToken | null {
  const raw = localStorage.getItem(TOKEN_KEY);
  if (!raw) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    parsed = null;
  }
  const candidate = parsed as StoredToken | null;
  if (!candidate || typeof candidate.token !== 'string' || typeof candidate.expiresAt !== 'number') {
    // Left over from an older build, or hand-edited; treat it as no token at all.
    clearToken();
    return null;
  }
  return candidate;
}

export function getToken(): string {
  const stored = readStoredToken();
  if (!stored) return '';
  if (stored.expiresAt <= Date.now()) {
    clearToken();
    return '';
  }
  // Slide the window once it is half spent, so regular use never hits the expiry
  // while an abandoned browser still forgets the token on schedule.
  if (stored.expiresAt - Date.now() < TOKEN_TTL_MS / 2) {
    setToken(stored.token);
  }
  return stored.token;
}

export function setToken(token: string): void {
  const stored: StoredToken = { token, expiresAt: Date.now() + TOKEN_TTL_MS };
  localStorage.setItem(TOKEN_KEY, JSON.stringify(stored));
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
  // Pre-persistence builds kept the token here; drop it so it cannot resurface.
  sessionStorage.removeItem(TOKEN_KEY);
}

/** Notifies the app shell so a 401/403 can bounce back to the login screen. */
const unauthorizedListeners = new Set<() => void>();

export function onUnauthorized(listener: () => void): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  headers.set('Authorization', `Bearer ${getToken()}`);
  if (init.body !== undefined) {
    headers.set('Content-Type', 'application/json');
  }

  const response = await fetch(path, { ...init, headers });

  if (response.status === 401 || response.status === 403) {
    clearToken();
    unauthorizedListeners.forEach((listener) => listener());
  }

  if (response.status === 204) {
    return undefined as T;
  }

  const text = await response.text();
  const payload = text ? JSON.parse(text) : undefined;

  if (!response.ok) {
    const error = payload?.error;
    throw new ApiError(
      response.status,
      error?.code ?? 'http_error',
      error?.message ?? `HTTP ${response.status}`,
      error?.details,
    );
  }
  return payload as T;
}

export const api = {
  get: <T,>(path: string) => request<T>(path),
  put: <T,>(path: string, body: unknown) => request<T>(path, { method: 'PUT', body: JSON.stringify(body) }),
  post: <T,>(path: string, body: unknown) => request<T>(path, { method: 'POST', body: JSON.stringify(body) }),
  delete: <T,>(path: string) => request<T>(path, { method: 'DELETE' }),
};

/** Cheap authenticated call used to validate a pasted token at login. */
export async function verifyToken(token: string): Promise<boolean> {
  const response = await fetch('/api/v2/jobs', { headers: { Authorization: `Bearer ${token}` } });
  return response.ok;
}
