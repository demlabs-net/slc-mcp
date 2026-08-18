// Auth-состояние SPA (порт легаси web-ui/src/lib/auth.ts).
// Режимы сервера: seat (X-Seat-ID, логин не нужен) и full (JWT + Yandex).
// Boot-проверка: GET /api/auth/me — 200 с user → full; 200 с auth_mode=seat
// → seat; 401 → full без сессии → показать Login.

import { writable, derived } from 'svelte/store';
import { api, getAccessToken, setAccessToken } from './api';

export interface User {
  user_id: string;
  username: string;
  email: string;
  groups: string[];
  permissions?: string[];
}

export interface AuthState {
  user: User | null;
  accessToken: string | null;
  refreshToken: string | null;
  mode: 'seat' | 'full' | null;
  loading: boolean;
}

const STORAGE_KEY = 'slc_auth';

function loadFromStorage(): Partial<AuthState> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const data = JSON.parse(raw);
      return {
        user: data.user || null,
        accessToken: data.accessToken || null,
        refreshToken: data.refreshToken || null,
      };
    }
  } catch {}
  return { user: null, accessToken: null, refreshToken: null };
}

function saveToStorage(state: AuthState) {
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        user: state.user,
        accessToken: state.accessToken,
        refreshToken: state.refreshToken,
      }),
    );
  } catch {}
}

function clearStorage() {
  try { localStorage.removeItem(STORAGE_KEY); } catch {}
}

const stored = loadFromStorage();

export const auth = writable<AuthState>({
  user: stored.user || null,
  accessToken: stored.accessToken || null,
  refreshToken: stored.refreshToken || null,
  mode: null,
  loading: true,
});

export const isAuthenticated = derived(auth, $a => !!$a.accessToken && !!$a.user);
export const currentUser = derived(auth, $a => $a.user);
export const authMode = derived(auth, $a => $a.mode);
export const isAdmin = derived(auth, $a => {
  const groups = $a.user?.groups || [];
  return groups.includes('admins') || groups.includes('superadmins');
});
export const isSuperAdmin = derived(auth, $a => ($a.user?.groups || []).includes('superadmins'));

function apply(state: AuthState) {
  setAccessToken(state.accessToken);
  auth.set(state);
  saveToStorage(state);
}

export async function login(username: string, password: string): Promise<boolean> {
  try {
    const res = await api.auth.login(username, password);
    apply({
      user: res.user,
      accessToken: res.access_token,
      refreshToken: res.refresh_token,
      mode: 'full',
      loading: false,
    });
    return true;
  } catch (err: any) {
    console.error('Login failed:', err.message);
    return false;
  }
}

export async function refreshTokens(): Promise<boolean> {
  let refreshToken: string | null = null;
  auth.subscribe(s => { refreshToken = s.refreshToken; })();
  if (!refreshToken) return false;
  try {
    const res = await api.auth.refresh(refreshToken);
    apply({
      user: res.user,
      accessToken: res.access_token,
      refreshToken: res.refresh_token,
      mode: 'full',
      loading: false,
    });
    return true;
  } catch {
    logout();
    return false;
  }
}

export function logout() {
  try { api.auth.logout().catch(() => {}); } catch {}
  setAccessToken(null);
  auth.set({ user: null, accessToken: null, refreshToken: null, mode: 'full', loading: false });
  clearStorage();
}

/**
 * Boot-проверка: OAuth-callback → токены из localStorage → режим сервера.
 */
export async function checkAuth(): Promise<boolean> {
  // 1) OAuth callback (?auth_code=)
  if (await handleOAuthCallback()) return true;

  // 2) Сохранённые токены (full-режим)
  if (stored.accessToken) {
    setAccessToken(stored.accessToken);
    try {
      const res = await api.auth.me();
      apply({
        user: res.user,
        accessToken: stored.accessToken!,
        refreshToken: stored.refreshToken!,
        mode: 'full',
        loading: false,
      });
      return true;
    } catch {
      // Просрочен access → refresh; не вышло → login page
      setAccessToken(null);
      if (await refreshTokens()) return true;
      auth.set({ user: null, accessToken: null, refreshToken: stored.refreshToken, mode: 'full', loading: false });
      return false;
    }
  }

  // 3) Без токенов: /api/auth/me решает режим (seat-режим всегда 200)
  try {
    const res = await api.auth.me();
    if (res?.auth_mode === 'seat') {
      auth.set({ user: null, accessToken: null, refreshToken: null, mode: 'seat', loading: false });
      return true;
    }
    if (res?.user) {
      apply({ user: res.user, accessToken: null, refreshToken: null, mode: 'full', loading: false });
      return true;
    }
  } catch {}
  // 401 → full-режим, нужен логин
  auth.set({ user: null, accessToken: null, refreshToken: null, mode: 'full', loading: false });
  return false;
}

/**
 * OAuth callback: /?auth_code=… → POST /api/auth/exchange → токены.
 * Токены никогда не появляются в URL (легаси-флоу).
 */
export async function handleOAuthCallback(): Promise<boolean> {
  const urlParams = new URLSearchParams(window.location.search);
  const authCode = urlParams.get('auth_code');
  if (!authCode) return false;
  window.history.replaceState({}, '', window.location.pathname);
  try {
    const res = await api.auth.exchange(authCode);
    setAccessToken(res.access_token);
    const me = await api.auth.me();
    apply({
      user: me.user,
      accessToken: res.access_token,
      refreshToken: res.refresh_token,
      mode: 'full',
      loading: false,
    });
    window.location.hash = '#/dashboard';
    return true;
  } catch (err) {
    console.error('OAuth code exchange failed:', err);
    window.location.hash = '#/login?oauth_error=exchange_failed';
    return false;
  }
}

export function hasRole(...roles: string[]): boolean {
  let groups: string[] = [];
  auth.subscribe(s => { groups = s.user?.groups || []; })();
  return roles.some(r => groups.includes(r));
}

/** Access-токен для EventSource и прочего (не через fetch). */
export function tokenForStream(): string {
  return getAccessToken() || '';
}
