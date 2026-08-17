import type { JobRequest } from '../api/types';
import { statusLabel } from '../labels';

/** The class stays keyed by the English status; only the visible text is translated. */
export function StatusPill({ status }: { status: JobRequest['status'] }) {
  return <span className={`pill pill-${status}`}>{statusLabel(status)}</span>;
}
