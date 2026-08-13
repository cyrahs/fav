import type { ReactElement } from 'react';
import { describeCron } from './CronInput';
import {
  CheckboxField,
  CheckboxGroup,
  NumberField,
  Repeater,
  SecretField,
  SelectField,
  TextField,
  type Option,
} from './Field';
import { AzurLaneProxyTest } from './AzurLaneProxyTest';
import { BilibiliForm, validateBilibili } from './BilibiliForm';
import { CookieCloudTest } from './CookieCloudTest';
import { TelegramForm, validateTelegram } from './TelegramForm';
import {
  PathField,
  bool,
  list,
  num,
  patcher,
  record,
  str,
  type SectionFormProps,
} from './sectionFields';

function NikkeForm(props: SectionFormProps) {
  const set = patcher(props);
  return (
    <div className="field-grid">
      <PathField {...props} />
      <div className="field-checks">
        <CheckboxField
          label="启用运行时图层抓取"
          checked={bool(props.value, 'runtime_capture_enabled')}
          onChange={(next) => set('runtime_capture_enabled', next)}
          hint="需要 Playwright Chromium"
        />
        <CheckboxField
          label="强制刷新抓取缓存"
          checked={bool(props.value, 'runtime_capture_force_refresh')}
          onChange={(next) => set('runtime_capture_force_refresh', next)}
        />
      </div>
      <NumberField
        label="抓取超时（秒）"
        value={num(props.value, 'runtime_capture_timeout_seconds', 60)}
        onChange={(next) => set('runtime_capture_timeout_seconds', next)}
        step={0.5}
      />
    </div>
  );
}

const HANIME1_PERIODS: Option<string>[] = [
  { value: 'weekly', label: '周榜' },
  { value: 'monthly', label: '月榜' },
];

function Hanime1Form(props: SectionFormProps) {
  const set = patcher(props);
  const ranking = record(props.value, 'ranking');
  const setRanking = (key: string, next: unknown) => set('ranking', { ...ranking, [key]: next });
  const periods = list<string>(ranking, 'periods');
  const deepScan = record(ranking, 'deep_scan');
  const setDeepScan = (key: string, next: unknown) => setRanking('deep_scan', { ...deepScan, [key]: next });

  return (
    <div className="field-grid">
      <PathField {...props} />
      <TextField label="站点地址" value={str(props.value, 'host')} onChange={(next) => set('host', next)} mono />
      <SelectField
        label="字幕语言"
        value={str(props.value, 'user_lang', 'zhs')}
        options={[
          { value: 'zhs', label: '简体中文 (zhs)' },
          { value: 'zht', label: '繁體中文 (zht)' },
        ]}
        onChange={(next) => set('user_lang', next)}
      />

      <div className="subsection">
        <h4>排行榜抓取</h4>
        <div className="field-grid">
          <CheckboxField
            label="启用排行榜"
            checked={bool(ranking, 'enabled')}
            onChange={(next) => setRanking('enabled', next)}
          />
          <CheckboxGroup
            label="榜单周期"
            values={periods}
            options={HANIME1_PERIODS}
            onChange={(next) => setRanking('periods', next)}
            error={periods.length === 0 ? '至少选择一个周期' : undefined}
          />
          <NumberField
            label="每个榜单抓取页数"
            value={num(ranking, 'pages', 1)}
            onChange={(next) => setRanking('pages', next)}
          />
        </div>
      </div>

      <div className="subsection">
        <h4>深度补扫</h4>
        <div className="field-grid">
          <CheckboxField
            label="新系列不足时向后补扫"
            checked={bool(deepScan, 'enabled')}
            onChange={(next) => setDeepScan('enabled', next)}
          />
          <NumberField
            label="每次扫描目标新系列数（可为小数）"
            value={num(deepScan, 'quota', 0.25)}
            onChange={(next) => setDeepScan('quota', next)}
            step={0.25}
          />
          <NumberField
            label="补扫最多额外页数"
            value={num(deepScan, 'max_extra_pages', 5)}
            onChange={(next) => setDeepScan('max_extra_pages', next)}
          />
        </div>
      </div>
    </div>
  );
}

const JANDAN_FAV_TYPES: Option<number>[] = [
  { value: 1, label: '无聊图 (wuliao)' },
  { value: 2, label: '随手拍 (snapshot)' },
  { value: 6, label: '妹子图 (girls)' },
];

function JandanForm(props: SectionFormProps) {
  const set = patcher(props);
  const favTypes = list<number>(props.value, 'fav_types');
  return (
    <div className="field-grid">
      <NumberField
        label="用户 ID"
        value={num(props.value, 'user_id')}
        onChange={(next) => set('user_id', next)}
        hint="字段 user_id"
      />
      <TextField label="API 地址" value={str(props.value, 'api_url')} onChange={(next) => set('api_url', next)} mono />
      <PathField {...props} />
      <CheckboxGroup
        label="收藏分类"
        values={favTypes}
        options={JANDAN_FAV_TYPES}
        onChange={(next) => set('fav_types', next)}
        hint="括号内是落盘的子目录名"
        error={favTypes.length === 0 ? '至少选择一个分类' : undefined}
      />
      <NumberField
        label="单次抓取上限"
        value={num(props.value, 'fav_num_limit', 45)}
        onChange={(next) => set('fav_num_limit', next)}
        hint="字段 fav_num_limit"
      />
    </div>
  );
}

interface KemonoCreator {
  service?: string;
  id?: string;
  name?: string;
}

function AzurLaneForm(props: SectionFormProps) {
  const set = patcher(props);
  const originProxy = str(props.value, 'origin_proxy');

  return (
    <div className="field-grid">
      <PathField {...props} />

      <SecretField
        label="源站代理"
        value={originProxy}
        onChange={(next) => set('origin_proxy', next)}
        invalid={!originProxy}
        hint={
          originProxy
            ? undefined
            : 'l2d.su 会封禁机房 IP，必须配置住宅代理，否则任务保持未就绪。格式：http://用户名:密码@主机:端口'
        }
      />
      <AzurLaneProxyTest originProxy={originProxy} />

      <NumberField
        label="源站请求间隔（秒）"
        value={num(props.value, 'origin_request_interval_seconds', 1)}
        onChange={(next) => set('origin_request_interval_seconds', next)}
        step={0.5}
        hint="只限制打到 l2d.su 的总速率；每个请求本来就用独立出口 IP。低于单请求耗时（约 2 秒）就不再有效果。"
      />
    </div>
  );
}

function KemonoForm(props: SectionFormProps) {
  const set = patcher(props);
  const creators = list<KemonoCreator>(props.value, 'creators');

  const update = (index: number, patch: Partial<KemonoCreator>) => {
    set(
      'creators',
      creators.map((creator, position) => (position === index ? { ...creator, ...patch } : creator)),
    );
  };

  return (
    <div className="field-grid">
      <PathField {...props} />

      <Repeater
        label="创作者"
        count={creators.length}
        addLabel="添加创作者"
        empty="还没有创作者，Kemono 任务会保持未就绪。"
        hint="service 与 id 取自作品页地址：/{service}/user/{id}"
        onAdd={() => set('creators', [...creators, { service: 'fanbox', id: '', name: '' }])}
      >
        <table className="table compact">
          <thead>
            <tr>
              <th>Service</th>
              <th>用户 ID</th>
              <th>名称（目录名）</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {creators.map((creator, index) => (
              // eslint-disable-next-line react/no-array-index-key -- rows have no stable id until saved
              <tr key={index}>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={creator.service ?? ''}
                    placeholder="fanbox"
                    onChange={(event) => update(index, { service: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="text"
                    className="mono-input"
                    value={creator.id ?? ''}
                    onChange={(event) => update(index, { id: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="text"
                    value={creator.name ?? ''}
                    onChange={(event) => update(index, { name: event.target.value })}
                  />
                </td>
                <td className="actions">
                  <button
                    type="button"
                    className="danger"
                    onClick={() =>
                      set(
                        'creators',
                        creators.filter((_, position) => position !== index),
                      )
                    }
                  >
                    删除
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Repeater>
    </div>
  );
}

interface CookieCloudCredentials {
  server_url?: string;
  uuid?: string;
  password?: string;
}

const TWITTER_USERNAME_RE = /^[A-Za-z0-9_]+$/;

function TwitterForm(props: SectionFormProps) {
  const set = patcher(props);
  const username = str(props.value, 'username');
  const usernameOk = TWITTER_USERNAME_RE.test(username);
  const cookiecloud = record(props.value, 'cookiecloud') as CookieCloudCredentials;
  const setCookieCloud = (patch: Partial<CookieCloudCredentials>) =>
    set('cookiecloud', { ...cookiecloud, ...patch });

  return (
    <div className="field-grid">
      <TextField
        label="自己的用户名"
        value={username}
        onChange={(next) => set('username', next)}
        mono
        placeholder="不带 @"
        invalid={!usernameOk}
        hint="点赞列表只有本人能看，必须和下面 CookieCloud 里的登录账号是同一个。"
        error={username && !usernameOk ? '只能包含字母、数字、下划线' : undefined}
      />
      <PathField {...props} />
      <TextField
        label="视频保存路径"
        value={str(props.value, 'video_path')}
        onChange={(next) => set('video_path', next)}
        mono
        placeholder="留空则和图片放在一起"
        hint="视频和 GIF 单独存放的位置（X 把 GIF 也存成视频）。留空则跟随上面的保存路径。"
      />

      <div className="subsection">
        <h4>CookieCloud 凭据</h4>
        <div className="field-grid">
          <TextField
            label="服务地址"
            value={cookiecloud.server_url ?? ''}
            onChange={(next) => setCookieCloud({ server_url: next })}
            mono
            placeholder="https://cookiecloud.example.com/"
          />
          <TextField
            label="UUID"
            value={cookiecloud.uuid ?? ''}
            onChange={(next) => setCookieCloud({ uuid: next })}
            mono
          />
          <SecretField
            label="密码"
            value={cookiecloud.password ?? ''}
            onChange={(next) => setCookieCloud({ password: next })}
            hint="浏览器插件需要同步 x.com 的 auth_token 和 ct0"
          />
          <CookieCloudTest
            source="twitter"
            serverUrl={cookiecloud.server_url ?? ''}
            uuid={cookiecloud.uuid ?? ''}
            password={cookiecloud.password ?? ''}
          />
        </div>
      </div>

      <div className="subsection">
        <h4>抓取节奏</h4>
        <div className="field-grid">
          <NumberField
            label="请求间隔（秒）"
            value={num(props.value, 'sleep_request_seconds', 2)}
            onChange={(next) => set('sleep_request_seconds', next)}
            step={0.5}
            hint="调低会更快撞上 X 的限流；首次全量回填尤其不建议低于 2 秒。"
          />
          <NumberField
            label="连续多少个已存档文件后停止"
            value={num(props.value, 'abort_after', 20)}
            onChange={(next) => set('abort_after', next)}
            hint="只在首次全量回填完成后生效；之前每轮都会走完整个点赞列表。"
          />
          <TextField
            label="代理"
            value={str(props.value, 'proxy')}
            onChange={(next) => set('proxy', next)}
            mono
            placeholder="http://host:port"
            hint="留空则直连（或走全局 HTTP_PROXY）"
          />
          <div className="field-checks">
            <CheckboxField
              label="包含转推"
              checked={bool(props.value, 'include_retweets', true)}
              onChange={(next) => set('include_retweets', next)}
              hint="点赞的转推按原作者归档"
            />
            <CheckboxField
              label="包含视频"
              checked={bool(props.value, 'include_videos', true)}
              onChange={(next) => set('include_videos', next)}
              hint="关闭后只抓图片；GIF 算视频"
            />
          </div>
        </div>
      </div>
    </div>
  );
}

function XiaohongshuForm(props: SectionFormProps) {
  const set = patcher(props);
  const allowDirect = bool(props.value, 'allow_direct_connection');

  return (
    <div className="field-grid">
      <PathField {...props} />
      <TextField
        label="视频保存路径"
        value={str(props.value, 'video_path')}
        onChange={(next) => set('video_path', next)}
        mono
        placeholder="留空则和图片放在一起"
        hint="视频笔记和实况图片的视频部分单独存放的位置。留空则跟随上面的保存路径。"
      />

      <div className="subsection">
        <h4>出口与登录</h4>
        <div className="field-grid">
          <SecretField
            label="代理"
            value={str(props.value, 'proxy')}
            onChange={(next) => set('proxy', next)}
            hint="小红书屏蔽了大部分机房 IP 段。带账号密码时必须用 http(s)：Chromium 不支持 SOCKS 认证。"
          />
          <CheckboxField
            label="允许不走代理直连"
            checked={allowDirect}
            onChange={(next) => set('allow_direct_connection', next)}
            hint="只有当这台机器本身就在住宅网络时才勾。从机房直连曾导致账号全端掉登录。"
          />
          <TextField
            label="浏览器 profile 路径"
            value={str(props.value, 'profile_path')}
            onChange={(next) => set('profile_path', next)}
            mono
            placeholder="./data/xiaohongshu-profile"
            hint="登录态存在这里，必须放在持久卷上，否则每次重启都要重新扫码。"
          />
          <TextField
            label="自己的用户 ID"
            value={str(props.value, 'user_id')}
            onChange={(next) => set('user_id', next)}
            mono
            placeholder="留空自动获取"
            hint="点赞列表在 /user/profile/<id>。留空则登录后从页面读一次并记住。"
          />
          <NumberField
            label="等待扫码（秒）"
            value={num(props.value, 'login_wait_seconds', 240)}
            onChange={(next) => set('login_wait_seconds', next)}
            hint="未登录时会把二维码发到 Telegram 并等这么久。等待期间其他手动触发会排队。"
          />
        </div>
      </div>

      <div className="subsection">
        <h4>抓取节奏</h4>
        <div className="field-grid">
          <NumberField
            label="请求间隔（秒）"
            value={num(props.value, 'sleep_request_seconds', 3)}
            onChange={(next) => set('sleep_request_seconds', next)}
            step={0.5}
            hint="小红书风控读请求节奏，这个值更值得调高而不是调低。"
          />
          <NumberField
            label="连续多少页全是旧内容后停止"
            value={num(props.value, 'abort_after', 2)}
            onChange={(next) => set('abort_after', next)}
            hint="点赞列表按时间倒序，连续几页都已入库就说明这一轮追上了。首次运行会走完整个列表。"
          />
          <NumberField
            label="每轮最多翻多少页"
            value={num(props.value, 'max_pages_per_run', 40)}
            onChange={(next) => set('max_pages_per_run', next)}
            hint="内存阀：点赞页只增不减地往下长。触顶后下一轮从这里继续。"
          />
          <CheckboxField
            label="媒体下载也走代理"
            checked={bool(props.value, 'proxy_media')}
            onChange={(next) => set('proxy_media', next)}
            hint="图片来自无需登录的 CDN，默认直连；只有 CDN 也拦机房 IP 时才需要打开。"
          />
        </div>
      </div>
    </div>
  );
}

function TelegramNotificationForm(props: SectionFormProps) {
  const set = patcher(props);
  const threadId = props.value.message_thread_id;
  const hasThread = threadId !== null && threadId !== undefined;

  return (
    <div className="field-grid">
      <CheckboxField
        label="启用通知投递"
        checked={bool(props.value, 'enabled')}
        onChange={(next) => set('enabled', next)}
        hint="关闭后通知仍会入队，只是不投递"
      />
      <SecretField
        label="Bot Token"
        value={str(props.value, 'bot_token')}
        onChange={(next) => set('bot_token', next)}
        hint="从 @BotFather 获取"
      />
      <TextField
        label="Chat ID"
        value={str(props.value, 'chat_id')}
        onChange={(next) => set('chat_id', next)}
        mono
        placeholder="-1001234567890 或 @channelname"
        hint="机器人必须已经在这个对话里"
      />
      <div className="field-checks">
        <CheckboxField
          label="发到话题（Topic）"
          checked={hasThread}
          onChange={(next) => set('message_thread_id', next ? 1 : null)}
          hint="仅适用于开启了话题的超级群"
        />
      </div>
      {hasThread && (
        <NumberField
          label="Message Thread ID"
          value={num(props.value, 'message_thread_id', 1)}
          onChange={(next) => set('message_thread_id', next)}
        />
      )}
    </div>
  );
}

export const SECTION_FORMS: Record<string, (props: SectionFormProps) => ReactElement> = {
  'web.bilibili': BilibiliForm,
  'web.telegram': TelegramForm,
  'web.nikke': NikkeForm,
  'web.azurlane': AzurLaneForm,
  'web.hanime1': Hanime1Form,
  'web.jandan': JandanForm,
  'web.kemono': KemonoForm,
  'web.twitter': TwitterForm,
  'web.xiaohongshu': XiaohongshuForm,
  'notifications.telegram': TelegramNotificationForm,
};

/**
 * Sections with nothing to configure beyond cron/enabled, which live on the jobs
 * page. Their remaining field is `path`; it keeps its default and is reachable
 * through `PUT /api/v2/settings/{section}` if a deployment ever needs to move it.
 */
export const JOBS_PAGE_ONLY_SECTIONS: ReadonlySet<string> = new Set([
  'web.stellasora',
  'web.bd2',
]);

/* ---------- client-side validation ---------- */

/** Find every NaN a numeric input left behind, so the save button can block on it. */
function collectNaNPaths(value: unknown, path = ''): string[] {
  if (typeof value === 'number') {
    return Number.isFinite(value) ? [] : [path || '(值)'];
  }
  if (Array.isArray(value)) {
    return value.flatMap((item, index) => collectNaNPaths(item, `${path}[${index}]`));
  }
  if (value && typeof value === 'object') {
    return Object.entries(value).flatMap(([key, item]) => collectNaNPaths(item, path ? `${path}.${key}` : key));
  }
  return [];
}

/**
 * Mirror the constraints most likely to be tripped in the form so the user gets a
 * message before the round trip. The backend stays the authority — anything not
 * caught here still comes back as a 422.
 */
export function validateSection(section: string, value: Record<string, unknown>): string[] {
  const issues = collectNaNPaths(value).map((path) => `${path} 不是合法数字`);

  if (typeof value.cron === 'string') {
    const described = describeCron(value.cron);
    if (!described.valid) {
      issues.push(`cron：${described.text}`);
    }
  }

  if (section === 'web.bilibili') {
    issues.push(...validateBilibili(value));
  }

  if (section === 'web.telegram') {
    issues.push(...validateTelegram(value));
  }

  if (section === 'web.jandan' && list<number>(value, 'fav_types').length === 0) {
    issues.push('收藏分类至少选择一个');
  }

  if (section === 'web.hanime1') {
    const ranking = record(value, 'ranking');
    if (list<string>(ranking, 'periods').length === 0) {
      issues.push('排行榜周期至少选择一个');
    }
    if (num(ranking, 'pages', 1) < 1) {
      issues.push('排行榜页数至少为 1');
    }
    const deepScan = record(ranking, 'deep_scan');
    const quota = num(deepScan, 'quota', 0.25);
    if (!Number.isFinite(quota) || quota <= 0) {
      issues.push('深度补扫目标新系列数必须大于 0');
    }
    if (num(deepScan, 'max_extra_pages', 5) < 1) {
      issues.push('深度补扫额外页数至少为 1');
    }
  }

  if (section === 'notifications.telegram') {
    const enabled = value.enabled === true;
    if (enabled && !str(value, 'bot_token').trim()) {
      issues.push('启用后必须填 Bot Token');
    }
    if (enabled && !str(value, 'chat_id').trim()) {
      issues.push('启用后必须填 Chat ID');
    }
    const thread = value.message_thread_id;
    if (thread !== null && thread !== undefined && !(Number.isInteger(thread) && (thread as number) > 0)) {
      issues.push('Message Thread ID 必须是正整数');
    }
  }

  if (section === 'web.twitter') {
    const username = str(value, 'username').trim().replace(/^@/, '');
    if (username && !TWITTER_USERNAME_RE.test(username)) {
      issues.push('用户名只能包含字母、数字、下划线');
    }
    if (num(value, 'sleep_request_seconds', 2) < 0) {
      issues.push('请求间隔不能为负数');
    }
    if (num(value, 'abort_after', 20) < 1) {
      issues.push('停止阈值至少为 1');
    }
  }

  if (section === 'web.kemono') {
    list<KemonoCreator>(value, 'creators').forEach((creator, index) => {
      if (!creator.service?.trim() || !creator.id?.trim() || !creator.name?.trim()) {
        issues.push(`创作者 #${index + 1} 的 service / id / 名称都不能为空`);
      }
    });
  }

  return issues;
}
