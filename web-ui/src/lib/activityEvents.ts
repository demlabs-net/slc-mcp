import { readCssVar } from './theme';

/** Known activity event types — colours come from CSS vars (theme-aware). */
export const ACTIVITY_EVENT_TYPES = [
  { key: 'tool_call', label: 'MCP Requests', colorVar: '--event-tool-call', group: 'agent' as const },
  { key: 'document_change', label: 'Documents', colorVar: '--event-document-change', group: 'agent' as const },
  { key: 'embedding', label: 'Embeddings', colorVar: '--event-embedding', group: 'server' as const },
  { key: 'ollama_call', label: 'LLM Calls', colorVar: '--event-ollama-call', group: 'server' as const },
  { key: 'agent_run', label: 'Agent Runs', colorVar: '--event-agent-run', group: 'server' as const },
  { key: 'timer_fired', label: 'Timers', colorVar: '--event-timer-fired', group: 'server' as const },
  { key: 'notification', label: 'Notifications', colorVar: '--event-notification', group: 'server' as const },
  { key: 'error', label: 'Errors', colorVar: '--event-error', group: 'both' as const },
] as const;

export type ActivityEventGroup = 'agent' | 'server' | 'both';

export interface ActivityEventType {
  key: string;
  label: string;
  colorVar: string;
  group: ActivityEventGroup;
}

/** Resolve event metadata; unknown types fall back to defaults. */
export function eventMeta(key: string): ActivityEventType {
  const known = ACTIVITY_EVENT_TYPES.find(t => t.key === key);
  if (known) return known;
  return { key, label: key, colorVar: '--event-default', group: 'both' };
}

/** All event types present in *buckets*, known + dynamic unknowns. */
export function eventTypesFromBuckets(buckets: { event_type: string }[]): ActivityEventType[] {
  const keys = [...new Set(buckets.map(b => b.event_type))];
  return keys.map(eventMeta);
}

export function eventLabel(key: string): string {
  return eventMeta(key).label;
}

/** Solid stroke/fill colour for Chart.js (respects current theme). */
export function eventColor(key: string): string {
  const varName = eventMeta(key).colorVar;
  return readCssVar(varName, readCssVar('--event-default', '#9ca3af'));
}

/** Semi-transparent fill for line/area charts. */
export function eventFillColor(key: string, alpha = 0.22): string {
  const solid = eventColor(key);
  if (solid.startsWith('#') && solid.length === 7) {
    const r = parseInt(solid.slice(1, 3), 16);
    const g = parseInt(solid.slice(3, 5), 16);
    const b = parseInt(solid.slice(5, 7), 16);
    return `rgba(${r}, ${g}, ${b}, ${alpha})`;
  }
  return solid;
}

/** Inline style for type badges in lists. */
export function eventBadgeStyle(key: string): string {
  const c = eventColor(key);
  return `background: color-mix(in srgb, ${c} 14%, transparent); color: ${c}`;
}
