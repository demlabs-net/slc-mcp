/**
 * Parse a date string from the backend (stored in UTC but serialized
 * as naive ISO-8601 without timezone suffix) into a proper Date object.
 */
export function utc(raw: string | null | undefined): Date | null {
  if (!raw) return null;
  const s = raw.trim();
  if (/[Zz+\-]\d{2}:?\d{2}$/.test(s) || s.endsWith('Z')) return new Date(s);
  return new Date(s.replace(' ', 'T') + 'Z');
}

/** Format a UTC date string to client-local date+time. */
export function fmtDateTime(raw: string | null | undefined): string {
  const d = utc(raw);
  if (!d || isNaN(d.getTime())) return '—';
  return d.toLocaleString();
}

/** Format to client-local date only. */
export function fmtDate(raw: string | null | undefined): string {
  const d = utc(raw);
  if (!d || isNaN(d.getTime())) return '—';
  return d.toLocaleDateString();
}

/** Format to client-local time only. */
export function fmtTime(raw: string | null | undefined): string {
  const d = utc(raw);
  if (!d || isNaN(d.getTime())) return '—';
  return d.toLocaleTimeString();
}

/** Relative time (e.g. "5m ago", "2h ago"). */
export function fmtRelative(raw: string | null | undefined): string {
  const d = utc(raw);
  if (!d || isNaN(d.getTime())) return '—';
  const diffMs = Date.now() - d.getTime();
  const mins = Math.floor(diffMs / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  return `${days}d ago`;
}
