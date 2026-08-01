import cronstrue from 'cronstrue/i18n';
import { useMemo } from 'react';

interface CronDescription {
  text: string;
  valid: boolean;
}

/** Render a 5-field crontab expression as a Chinese sentence. */
export function describeCron(expression: string): CronDescription {
  const trimmed = expression.trim();
  if (!trimmed) {
    return { text: '请输入 cron 表达式', valid: false };
  }
  // The backend uses standard 5-field crontab; reject other widths up front so
  // the hint matches what the scheduler will actually accept.
  if (trimmed.split(/\s+/).length !== 5) {
    return { text: 'cron 必须是 5 段（分 时 日 月 周）', valid: false };
  }
  try {
    return { text: cronstrue.toString(trimmed, { locale: 'zh_CN', use24HourTimeFormat: true }), valid: true };
  } catch (error) {
    return { text: error instanceof Error ? error.message : '无法解析该 cron 表达式', valid: false };
  }
}

interface CronInputProps {
  value: string;
  onChange: (value: string) => void;
  id?: string;
  disabled?: boolean;
}

export function CronInput({ value, onChange, id, disabled }: CronInputProps) {
  const description = useMemo(() => describeCron(value), [value]);

  return (
    <div className="cron-input">
      <input
        id={id}
        type="text"
        value={value}
        disabled={disabled}
        spellCheck={false}
        onChange={(event) => onChange(event.target.value)}
        placeholder="*/30 * * * *"
        aria-describedby={id ? `${id}-hint` : undefined}
        aria-invalid={!description.valid}
      />
      <p id={id ? `${id}-hint` : undefined} className={description.valid ? 'cron-hint' : 'cron-hint cron-hint-error'}>
        {description.text}
      </p>
    </div>
  );
}
