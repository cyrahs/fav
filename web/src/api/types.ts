export interface Job {
  key: string;
  name: string;
  enabled: boolean;
  notify: boolean;
  cron: string;
  section: string;
  missing_fields: string[];
}
export interface JobRequest {
  id: number;
  target: string;
  kind: string;
  status: 'pending' | 'running' | 'succeeded' | 'failed' | 'rejected';
  requested_at: string;
  started_at: string | null;
  finished_at: string | null;
  result: string;
  error: string;
}
export interface SettingsSection {
  section: string;
  value: Record<string, unknown>;
  missing_fields: string[];
}
export interface ArchiveSourceStat {
  source: string;
  name: string;
  total: number;
  latest_at: string | null;
}
export interface ArchiveItem {
  source: string;
  id: string;
  title: string;
  subtitle: string;
  created_at: string | null;
  url: string | null;
  fields: Record<string, unknown>;
}
export interface Hanime1Seed {
  video_id: string;
  title: string;
  label: string;
  added_from_video_id: string;
  video_count: number;
  created_at: string | null;
  updated_at: string | null;
  last_scanned_at: string | null;
  last_scan_error: string;
  watch_url: string;
}
export interface Hanime1Author {
  author_id: string;
  name: string;
  author_url: string;
  video_count: number;
  created_at: string | null;
  updated_at: string | null;
  last_scanned_at: string | null;
  last_scan_error: string;
}
export interface KemonoCreatorResolved {
  service: string;
  id: string;
  name: string;
}
export interface ListResponse<T> {
  items: T[];
  total: number;
}
export interface PagedResponse<T> extends ListResponse<T> {
  limit: number;
  offset: number;
}
export interface Health {
  status: string;
  generated_at: string;
}
export interface Readiness {
  status: 'ok' | 'degraded';
  generated_at: string;
  checks: Record<string, { status: string; code: string; message: string; sampled_targets: number }>;
}

export interface AzurLaneProxyTestResult {
  ok: boolean;
  code: string;
  message: string;
  exit_ip: string;
}

export interface RedNoteProxyTestResult {
  ok: boolean;
  code: string;
  message: string;
  exit_ip: string;
  direct: boolean;
}

export interface CookieCloudTestResult {
  ok: boolean;
  code: string;
  message: string;
  domain_count: number;
  domain_cookie_count: number;
  missing_cookies: string[];
}

export interface TelegramNotificationTest {
  status: 'delivered';
  message_id: number | null;
  warnings: string[];
}
