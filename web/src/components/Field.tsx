import { useEffect, useId, useState, type ReactNode } from 'react';

/**
 * Form primitives for the settings pages.
 *
 * Numeric inputs keep their own text state so a half-typed value ("-", "") is not
 * fought by the controlled value, and emit NaN while the text is not a number.
 * `collectNaNPaths` in sectionForms.tsx finds those and blocks the save, which
 * keeps the invalid-state plumbing out of every individual form.
 */

interface FieldShellProps {
  label: string;
  htmlFor?: string;
  hint?: ReactNode;
  error?: ReactNode;
  children: ReactNode;
}

export function FieldShell({ label, htmlFor, hint, error, children }: FieldShellProps) {
  return (
    <div className="field">
      <label htmlFor={htmlFor}>{label}</label>
      {children}
      {hint && <p className="field-hint">{hint}</p>}
      {error && <p className="warn">{error}</p>}
    </div>
  );
}

interface TextFieldProps {
  label: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  hint?: ReactNode;
  error?: ReactNode;
  mono?: boolean;
  invalid?: boolean;
}

export function TextField({ label, value, onChange, placeholder, hint, error, mono, invalid }: TextFieldProps) {
  const id = useId();
  return (
    <FieldShell label={label} htmlFor={id} hint={hint} error={error}>
      <input
        id={id}
        type="text"
        className={mono ? 'mono-input' : undefined}
        value={value}
        placeholder={placeholder}
        spellCheck={false}
        aria-invalid={invalid || undefined}
        onChange={(event) => onChange(event.target.value)}
      />
    </FieldShell>
  );
}

interface NumberFieldProps {
  label: string;
  value: number;
  onChange: (value: number) => void;
  placeholder?: string;
  hint?: ReactNode;
  step?: number;
  invalid?: boolean;
}

export function NumberField({ label, value, onChange, placeholder, hint, step, invalid }: NumberFieldProps) {
  const id = useId();
  const [text, setText] = useState(() => (Number.isFinite(value) ? String(value) : ''));

  // Re-sync only when an outside change (reset, refetch) actually moves the value,
  // so typing "1." or "-" is never clobbered mid-keystroke.
  useEffect(() => {
    if (Number.isFinite(value) && Number(text) !== value) {
      setText(String(value));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- text is intentionally not a trigger
  }, [value]);

  const empty = text.trim() === '';
  return (
    <FieldShell
      label={label}
      htmlFor={id}
      hint={hint}
      error={!Number.isFinite(value) ? (empty ? '不能为空' : '必须是数字') : undefined}
    >
      <input
        id={id}
        type="text"
        inputMode={step && !Number.isInteger(step) ? 'decimal' : 'numeric'}
        className="mono-input"
        value={text}
        placeholder={placeholder}
        spellCheck={false}
        aria-invalid={!Number.isFinite(value) || invalid || undefined}
        onChange={(event) => {
          const next = event.target.value;
          setText(next);
          onChange(next.trim() === '' ? Number.NaN : Number(next));
        }}
      />
    </FieldShell>
  );
}

interface CheckboxFieldProps {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  hint?: ReactNode;
}

export function CheckboxField({ label, checked, onChange, hint }: CheckboxFieldProps) {
  const id = useId();
  return (
    <div className="field field-check">
      <label className="switch" htmlFor={id}>
        <input id={id} type="checkbox" checked={checked} onChange={(event) => onChange(event.target.checked)} />
        <span>{label}</span>
      </label>
      {hint && <p className="field-hint">{hint}</p>}
    </div>
  );
}

export interface Option<T> {
  value: T;
  label: string;
}

interface SelectFieldProps<T extends string> {
  label: string;
  value: T;
  options: Option<T>[];
  onChange: (value: T) => void;
  hint?: ReactNode;
}

export function SelectField<T extends string>({ label, value, options, onChange, hint }: SelectFieldProps<T>) {
  const id = useId();
  return (
    <FieldShell label={label} htmlFor={id} hint={hint}>
      <select id={id} value={value} onChange={(event) => onChange(event.target.value as T)}>
        {options.map((option) => (
          <option key={String(option.value)} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </FieldShell>
  );
}

interface CheckboxGroupProps<T extends string | number> {
  label: string;
  values: T[];
  options: Option<T>[];
  onChange: (values: T[]) => void;
  hint?: ReactNode;
  error?: ReactNode;
}

/** Multi-select rendered as checkboxes; emits values in the declared option order. */
export function CheckboxGroup<T extends string | number>({
  label,
  values,
  options,
  onChange,
  hint,
  error,
}: CheckboxGroupProps<T>) {
  const selected = new Set<T>(values);
  return (
    <FieldShell label={label} hint={hint} error={error}>
      <div className="checkbox-group">
        {options.map((option) => (
          <label key={String(option.value)} className="switch">
            <input
              type="checkbox"
              checked={selected.has(option.value)}
              onChange={(event) => {
                const next = new Set(selected);
                if (event.target.checked) {
                  next.add(option.value);
                } else {
                  next.delete(option.value);
                }
                onChange(options.filter((candidate) => next.has(candidate.value)).map((candidate) => candidate.value));
              }}
            />
            <span>{option.label}</span>
          </label>
        ))}
      </div>
    </FieldShell>
  );
}

interface SecretFieldProps {
  label: string;
  value: string;
  onChange: (value: string) => void;
  hint?: ReactNode;
  invalid?: boolean;
}

/**
 * A masked value read back from the API is not a secret, so it stays visible —
 * that is the only way to tell "unchanged" from "empty". Once the user types a
 * real value the input switches to a password field.
 */
export function SecretField({ label, value, onChange, hint, invalid }: SecretFieldProps) {
  const id = useId();
  const masked = value.includes('•');
  return (
    <FieldShell
      label={label}
      htmlFor={id}
      hint={hint ?? (masked ? '已保存，留空或保持此掩码即不修改' : undefined)}
    >
      <input
        id={id}
        type={masked ? 'text' : 'password'}
        className={masked ? 'mono-input' : undefined}
        value={value}
        spellCheck={false}
        autoComplete="off"
        aria-invalid={invalid || undefined}
        onChange={(event) => onChange(event.target.value)}
      />
    </FieldShell>
  );
}

interface RepeaterProps {
  label: string;
  hint?: ReactNode;
  addLabel: string;
  onAdd: () => void;
  empty: string;
  count: number;
  children: ReactNode;
}

/** Shared chrome for the editable lists (telegram accounts/channels, kemono creators). */
export function Repeater({ label, hint, addLabel, onAdd, empty, count, children }: RepeaterProps) {
  return (
    <div className="repeater">
      <div className="repeater-head">
        <h4>
          {label} <span className="muted">({count})</span>
        </h4>
        <button type="button" className="ghost" onClick={onAdd}>
          {addLabel}
        </button>
      </div>
      {hint && <p className="field-hint">{hint}</p>}
      {count === 0 ? <p className="muted">{empty}</p> : children}
    </div>
  );
}
