import { TextField } from './Field';

export interface SectionFormProps {
  value: Record<string, unknown>;
  onChange: (next: Record<string, unknown>) => void;
}

/* ---------- typed readers over the untyped section payload ---------- */

export function str(value: Record<string, unknown>, key: string, fallback = ''): string {
  const raw = value[key];
  return typeof raw === 'string' ? raw : fallback;
}

export function num(value: Record<string, unknown>, key: string, fallback = 0): number {
  const raw = value[key];
  if (typeof raw === 'number') {
    return raw;
  }
  if (typeof raw === 'string' && raw.trim() !== '') {
    return Number(raw);
  }
  return fallback;
}

export function bool(value: Record<string, unknown>, key: string, fallback = false): boolean {
  const raw = value[key];
  return typeof raw === 'boolean' ? raw : fallback;
}

export function list<T>(value: Record<string, unknown>, key: string): T[] {
  const raw = value[key];
  return Array.isArray(raw) ? (raw as T[]) : [];
}

export function record(value: Record<string, unknown>, key: string): Record<string, unknown> {
  const raw = value[key];
  return raw && typeof raw === 'object' && !Array.isArray(raw) ? (raw as Record<string, unknown>) : {};
}

/** Merge one key, leaving keys the form does not know about untouched. */
export function patcher({ value, onChange }: SectionFormProps) {
  return (key: string, next: unknown) => onChange({ ...value, [key]: next });
}

/* ---------- field groups shared by several sections ---------- */

/**
 * `cron` and `enabled` live on the jobs page, not here — but they are still part
 * of the section payload, and PUT replaces the whole section. Every form spreads
 * the original value, which is what carries them through untouched.
 */
export function PathField(props: SectionFormProps) {
  const set = patcher(props);
  return (
    <TextField
      label="保存路径"
      value={str(props.value, 'path')}
      onChange={(next) => set('path', next)}
      placeholder="collection/xxx"
      mono
      hint="相对路径基于容器工作目录"
    />
  );
}
