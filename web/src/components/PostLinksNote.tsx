/**
 * The post links WeChat and Telegram both save. The two sources read them through
 * the same backend module, so the list lives here once and each form shows it.
 */
export function PostLinksNote({ where }: { where: string }) {
  return (
    <div className="subsection">
      <h4>链接</h4>
      <p className="muted">{where}一条链接时，下载帖子里的图片和视频。支持：</p>
      <ul className="muted">
        <li>公众号文章</li>
        <li>百度贴吧帖子</li>
        <li>NGA 帖子（需要在「登录态」页登录 NGA）</li>
        <li>知乎回答、专栏文章、想法（需要在「登录态」页登录知乎）</li>
      </ul>
      <p className="muted">缺登录时链接会排队等待，不会耗尽重试次数。不支持的链接和没有可下载内容的消息会发通知（Telegram 只在勾了「报告未下载」的频道）。</p>
    </div>
  );
}
