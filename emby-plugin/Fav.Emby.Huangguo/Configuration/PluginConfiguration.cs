using System;
using MediaBrowser.Model.Plugins;

namespace Fav.Emby.Huangguo.Configuration
{
    public class PluginConfiguration : BasePluginConfiguration
    {
        /// <summary>The default: fav's Service, beside Emby in the cluster's media namespace.</summary>
        public const string DefaultFavUrl = "http://fav.media.svc.cluster.local";

        /// <summary>Where fav answers; <c>FAV_HUANGGUO_URL</c> when left empty.</summary>
        public string FavUrl { get; set; } = string.Empty;

        /// <summary>
        /// The bearer token of fav's <c>CREDENTIAL_CLIENTS</c> entry with the
        /// <c>huangguo-media</c> scope; <c>FAV_HUANGGUO_TOKEN</c> when left empty.
        /// </summary>
        public string Token { get; set; } = string.Empty;

        public string EffectiveFavUrl()
        {
            var url = FirstSet(FavUrl, Environment.GetEnvironmentVariable("FAV_HUANGGUO_URL"), DefaultFavUrl);
            return url.TrimEnd('/');
        }

        public string EffectiveToken()
        {
            return FirstSet(Token, Environment.GetEnvironmentVariable("FAV_HUANGGUO_TOKEN"), string.Empty);
        }

        private static string FirstSet(params string[] values)
        {
            foreach (var value in values)
            {
                if (!string.IsNullOrWhiteSpace(value))
                {
                    return value.Trim();
                }
            }

            return string.Empty;
        }
    }
}
