/**
 * Backend timestamps are UTC ISO-8601 with a trailing Z (api/helpers.py), which
 * is both unreadable and eight hours off for anyone east of Greenwich. Every
 * user-facing timestamp goes through here and renders in the viewer's zone.
 */
export function formatDateTime(iso: string | null | undefined): string {
  if (!iso) return '—';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const pad = (part: number) => String(part).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
