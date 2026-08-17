// SLC web UI — REST-клиент к slc-webui (прокси над MCP-сервером).
// Сессия = seat id: генерируется в браузере, хранится в localStorage,
// передаётся заголовком X-Seat-ID (сервер авто-создаёт сид).

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

async function req<T = any>(method: string, path: string, body?: any): Promise<T> {
  const resp = await fetch(path, {
    method,
    headers: {
      'Content-Type': 'application/json',
      'X-Seat-ID': getSeat(),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) {
    throw new Error(data?.error || `HTTP ${resp.status}`);
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
};
