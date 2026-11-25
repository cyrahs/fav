using System;
using System.Collections.Generic;
using Fav.Emby.Huangguo.Configuration;
using MediaBrowser.Common.Configuration;
using MediaBrowser.Common.Plugins;
using MediaBrowser.Model.Plugins;
using MediaBrowser.Model.Serialization;

namespace Fav.Emby.Huangguo
{
    /// <summary>
    /// Series and episode metadata and images for the Huangguo library, read from fav:
    /// the details and poster fav saved for each series and the thumbnail it took of each
    /// episode (fav's <c>/media/huangguo</c> routes).
    /// </summary>
    public class Plugin : BasePlugin<PluginConfiguration>, IHasWebPages
    {
        public const string ProviderName = "Huangguo";

        /// <summary>The provider id key a matched series carries (the site's video id).</summary>
        public const string ProviderKey = "Huangguo";

        public Plugin(IApplicationPaths applicationPaths, IXmlSerializer xmlSerializer)
            : base(applicationPaths, xmlSerializer)
        {
            Instance = this;
        }

        public static Plugin Instance { get; private set; }

        public override string Name => ProviderName;

        public override string Description => "黄果短剧的剧集信息、海报和集缩略图，从 fav 读取。";

        public override Guid Id => new Guid("4b8c8826-067f-4e5a-876f-6237f6861b05");

        public IEnumerable<PluginPageInfo> GetPages()
        {
            return new[]
            {
                new PluginPageInfo
                {
                    Name = "huangguo",
                    EmbeddedResourcePath = GetType().Namespace + ".Configuration.configPage.html",
                    IsMainConfigPage = true,
                },
                new PluginPageInfo
                {
                    Name = "huangguojs",
                    EmbeddedResourcePath = GetType().Namespace + ".Configuration.configPage.js",
                },
            };
        }
    }
}
