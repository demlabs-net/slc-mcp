// SLC web UI — REST-клиент к slc-mcp (/api/*, тот же процесс, что и MCP).
// Два режима сервера (SLC_AUTH):
// - seat (default): X-Seat-ID, сид генерируется в браузере;
// - full: Bearer-JWT (users/пароли + Yandex OAuth), сид выводится из юзера.
// При 401 (full-режим) — однофлайтовый refresh + повтор запроса.

const SEAT_KEY = 'slc_seat_id';

export function getSeat(): string {
  let seat = localStorage.getItem(SEAT_KEY);
  if (!seat) {
    seat = 'slc_web_' + Math.random().toString(36).slice(2, 14) + Date.now().toString(36);
    localStorage.setItem(SEAT_KEY, seat);
  }
  return seat;
}

export function resetSeat(): void {
  localStorage.removeItem(SEAT_KEY);
}

// Токен выставляется модулем auth.ts (login/refresh/logout).
let accessToken: string | null = null;
export function setAccessToken(t: string | null): void {
  accessToken = t;
}
export function getAccessToken(): string | null {
  return accessToken;
}

async function req<T = any>(method: string, path: string, body?: any, retried = false): Promise<T> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    'X-Seat-ID': getSeat(),
  };
  if (accessToken) headers['Authorization'] = `Bearer ${accessToken}`;
  const resp = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (resp.status === 401 && accessToken && !path.startsWith('/api/auth/') && !retried) {
    // 401 → одна попытка refresh (single-flight), потом повтор запроса.
    if (!refreshPromise) {
      refreshPromise = import('./auth').then(m => m.refreshTokens()).finally(() => { refreshPromise = null; });
    }
    const ok = await refreshPromise;
    if (ok) return req(method, path, body, true);
  }
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) {
    throw new Error(data?.error || data?.detail || `HTTP ${resp.status}`);
  }
  return data as T;
}

const qs = (params?: Record<string, any>) => {
  if (!params) return '';
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
};

export const api = {
  health: () => req('/api/health'),
  stats: () => req('/api/stats'),
  context: () => req('/api/context'),

  documents: {
    list: (params?: { category?: string; folder?: string; query?: string; limit?: number }) =>
      req(`/api/documents${qs(params)}`),
    get: (id: string) => req(`/api/documents/${encodeURIComponent(id)}`),
    create: (data: { document_id: string; category: string; content: string; folder?: string }) =>
      req('/api/documents', 'POST', data),
    update: (id: string, data: any) =>
      req(`/api/documents/${encodeURIComponent(id)}`, 'PUT', data),
    remove: (id: string, purge = false) =>
      req(`/api/documents/${encodeURIComponent(id)}?purge=${purge}`, 'DELETE'),
  },

  search: (query: string, limit = 10) =>
    req(`/api/search${qs({ q: query, limit })}`),

  tasks: {
    list: (params?: { status?: string; project_id?: string; limit?: number }) =>
      req(`/api/tasks${qs(params)}`),
    create: (data: { name: string; description?: string; project_id?: string }) =>
      req('/api/tasks', 'POST', data),
    update: (id: string, data: any) =>
      req(`/api/tasks/${encodeURIComponent(id)}`, 'PUT', data),
    remove: (id: string) => req(`/api/tasks/${encodeURIComponent(id)}`, 'DELETE'),
  },

  projects: {
    list: (params?: { status?: string; limit?: number }) =>
      req(`/api/projects${qs(params)}`),
    create: (data: { name: string; description?: string }) =>
      req('/api/projects', 'POST', data),
    update: (id: string, data: any) =>
      req(`/api/projects/${encodeURIComponent(id)}`, 'PUT', data),
    remove: (id: string) => req(`/api/projects/${encodeURIComponent(id)}`, 'DELETE'),
  },

  seats: {
    list: () => req('/api/seats'),
  },

  notifications: {
    list: () => req('/api/notifications'),
    pop: () => req('/api/notifications', 'POST'),
  },

  reminders: {
    list: () => req('/api/reminders'),
    create: (data: { content: string; remind_at: string; mind_type?: string }) =>
      req('/api/reminders', 'POST', data),
    cancel: (id: string) => req(`/api/reminders/${encodeURIComponent(id)}`, 'DELETE'),
  },

  focuses: {
    list: () => req('/api/focuses'),
    add: (data: { title: string; description?: string; priority?: number; mind_type?: string }) =>
      req('/api/focuses', 'POST', data),
    update: (id: string, data: any) =>
      req(`/api/focuses/${encodeURIComponent(id)}`, 'PUT', data),
    remove: (id: string) => req(`/api/focuses/${encodeURIComponent(id)}`, 'DELETE'),
  },

  // ── auth (полный порт легаси: users/JWT/Yandex OAuth) ──
  auth: {
    me: () => req('/api/auth/me'),
    login: (username: string, password: string) =>
      req('/api/auth/login', 'POST', { username, password }),
    register: (data: { username: string; email: string; password: string; groups?: string[] }) =>
      req('/api/auth/register', 'POST', data),
    refresh: (refresh_token: string) =>
      req('/api/auth/refresh', 'POST', { refresh_token }),
    logout: () => req('/api/auth/logout', 'POST'),
    exchange: (code: string) => req('/api/auth/exchange', 'POST', { code }),
    users: {
      list: () => req('/api/auth/users'),
      setGroups: (user_id: string, groups: string[]) =>
        req(`/api/auth/users/${encodeURIComponent(user_id)}/groups`, 'PUT', groups),
      setActive: (user_id: string, is_active: boolean) =>
        req(`/api/auth/users/${encodeURIComponent(user_id)}/active${qs({ is_active })}`, 'PUT', {}),
    },
    groups: () => req('/api/auth/groups'),
  },

  admin: {
    oauthRules: {
      list: () => req('/api/admin/oauth/rules'),
      create: (data: { type: string; value: string; default_groups?: string[]; description?: string }) =>
        req('/api/admin/oauth/rules', 'POST', data),
      update: (rule_id: string, data: any) =>
        req(`/api/admin/oauth/rules/${encodeURIComponent(rule_id)}`, 'PUT', data),
      remove: (rule_id: string) =>
        req(`/api/admin/oauth/rules/${encodeURIComponent(rule_id)}`, 'DELETE'),
    },
    ya360Status: () => req('/api/admin/oauth/ya360/status'),
  },
};
