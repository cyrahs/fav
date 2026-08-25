import type { ReactNode } from 'react';
import { useQuery } from '@tanstack/react-query';
import { api } from '../api/client';
import type { ListResponse, SettingsSection } from '../api/types';
import { SelectField } from './Field';
import { CookieCloudTest } from './CookieCloudTest';

export interface SharedCookieCloudConfig {
  name?: string;
  server_url?: string;
  uuid?: string;
  password?: string;
}

/** The saved entries of the shared `cookiecloud` section, from the settings query cache. */
export function useSharedCookieCloudConfigs(): SharedCookieCloudConfig[] {
  const settings = useQuery({
    queryKey: ['settings'],
    queryFn: () => api.get<ListResponse<SettingsSection>>('/api/v2/settings'),
  });
  const value = settings.data?.items.find((section) => section.section === 'cookiecloud')?.value;
  const configs = value?.configs;
  return Array.isArray(configs) ? (configs as SharedCookieCloudConfig[]) : [];
}

interface CookieCloudPickerProps {
  /** Which source's cookies the test button checks for. */
  source: string;
  /** The referenced config name; empty means not selected. */
  value: string;
  onChange: (name: string) => void;
  hint?: ReactNode;
}

/**
 * Reference to a named entry of the shared CookieCloud registry, edited at the
 * bottom of the settings page. The test button probes the *saved* shared
 * credentials against this source's cookie profile — unsaved edits in the
 * CookieCloud block are not visible here.
 */
export function CookieCloudPicker({ source, value, onChange, hint }: CookieCloudPickerProps) {
  const configs = useSharedCookieCloudConfigs();
  const selected = configs.find((config) => config.name === value);
  const orphaned = value !== '' && selected === undefined;

  const options = [
    { value: '', label: '— 未选择 —' },
    ...configs.map((config) => ({ value: config.name ?? '', label: config.name ?? '' })),
    ...(orphaned ? [{ value, label: `${value}（配置已不存在）` }] : []),
  ];

  return (
    <>
      <SelectField
        label="CookieCloud 配置"
        value={value}
        options={options}
        onChange={onChange}
        hint={
          <>
            {hint ? <>{hint} </> : null}
            在配置页底部的 CookieCloud 区块统一维护，这里按名称引用，多个来源可共用同一份。
          </>
        }
      />
      {orphaned && <p className="warn">引用的配置「{value}」已不存在，请重新选择。</p>}
      {selected && (
        <CookieCloudTest
          source={source}
          name={selected.name ?? ''}
          serverUrl={selected.server_url ?? ''}
          uuid={selected.uuid ?? ''}
          password={selected.password ?? ''}
        />
      )}
    </>
  );
}
