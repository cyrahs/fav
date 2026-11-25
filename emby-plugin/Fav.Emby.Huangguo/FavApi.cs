using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Threading;
using System.Threading.Tasks;
using MediaBrowser.Common.Net;
using MediaBrowser.Model.Logging;
using MediaBrowser.Model.Serialization;

namespace Fav.Emby.Huangguo
{
    /// <summary>A series as fav's <c>/media/huangguo/series</c> answers it.</summary>
    public class FavSeries
    {
        public long id { get; set; }
        public string title { get; set; }
        public string category { get; set; }
        public string dir_name { get; set; }
        public string plot { get; set; }
        public string author { get; set; }
        public string[] tags { get; set; }
        public double? score { get; set; }
        public string premiere_date { get; set; }
        public bool finished { get; set; }
        public string url { get; set; }
        public bool has_poster { get; set; }
        /// <summary>The poster's signed path, readable without the token; null without a poster.</summary>
        public string poster_url { get; set; }
        public FavEpisode[] episodes { get; set; }
    }

    public class FavEpisode
    {
        public int ep_num { get; set; }
        public string title { get; set; }
        public int duration { get; set; }
        public bool has_thumbnail { get; set; }
        /// <summary>The thumbnail's signed path, readable without the token; null without one.</summary>
        public string thumbnail_url { get; set; }
    }

    /// <summary>fav's Huangguo media routes, with the configured token.</summary>
    public class FavApi
    {
        private static readonly TimeSpan CacheFor = TimeSpan.FromMinutes(2);

        // One refresh asks for the same series once per episode: keep answers briefly.
        private static readonly ConcurrentDictionary<string, Tuple<DateTime, FavSeries>> Cache =
            new ConcurrentDictionary<string, Tuple<DateTime, FavSeries>>();

        private readonly IHttpClient _http;
        private readonly IJsonSerializer _json;
        private readonly ILogger _logger;

        public FavApi(IHttpClient http, IJsonSerializer json, ILogger logger)
        {
            _http = http;
            _json = json;
            _logger = logger;
        }

        private static string BaseUrl => Plugin.Instance.Configuration.EffectiveFavUrl() + "/media/huangguo";

        /// <summary>
        /// The series' poster. Emby fetches an image URL by itself, without the token
        /// (<c>/Images/Remote</c>, or an image it stored as a URL when first shown), so this is
        /// the signed path fav gives, which needs none; the bare route when fav gave none.
        /// </summary>
        public static string PosterUrl(FavSeries series) =>
            Signed(series.poster_url) ?? $"{BaseUrl}/series/{series.id}/poster";

        /// <summary>An episode's thumbnail, signed like <see cref="PosterUrl"/>.</summary>
        public static string ThumbnailUrl(FavSeries series, FavEpisode episode) =>
            Signed(episode.thumbnail_url) ?? $"{BaseUrl}/series/{series.id}/episodes/{episode.ep_num}/thumbnail";

        private static string Signed(string path) =>
            string.IsNullOrEmpty(path) ? null : Plugin.Instance.Configuration.EffectiveFavUrl() + path;

        public Task<FavSeries> SeriesById(string id, CancellationToken cancellationToken)
        {
            return Fetch($"{BaseUrl}/series/{Uri.EscapeDataString(id)}", cancellationToken);
        }

        /// <summary>
        /// The series saved in this folder: <c>/media/ai/短剧/&lt;series&gt;</c> is the
        /// series <c>&lt;series&gt;</c> of the category folder <c>短剧</c>.
        /// </summary>
        public Task<FavSeries> SeriesByFolder(string folder, CancellationToken cancellationToken)
        {
            if (string.IsNullOrEmpty(folder))
            {
                return Task.FromResult<FavSeries>(null);
            }

            var trimmed = folder.TrimEnd('/', '\\');
            var dirName = Path.GetFileName(trimmed);
            var category = Path.GetFileName(Path.GetDirectoryName(trimmed) ?? string.Empty);
            if (string.IsNullOrEmpty(dirName))
            {
                return Task.FromResult<FavSeries>(null);
            }

            var url = $"{BaseUrl}/series?dir_name={Uri.EscapeDataString(dirName)}";
            if (!string.IsNullOrEmpty(category))
            {
                url += $"&category={Uri.EscapeDataString(category)}";
            }

            return Fetch(url, cancellationToken);
        }

        /// <summary>The series by its provider id, else by its folder.</summary>
        public async Task<FavSeries> Find(string providerId, string folder, CancellationToken cancellationToken)
        {
            if (!string.IsNullOrEmpty(providerId))
            {
                var byId = await SeriesById(providerId, cancellationToken).ConfigureAwait(false);
                if (byId != null)
                {
                    return byId;
                }
            }

            return await SeriesByFolder(folder, cancellationToken).ConfigureAwait(false);
        }

        public Task<HttpResponseInfo> Image(string url, CancellationToken cancellationToken)
        {
            return _http.GetResponse(Options(url, cancellationToken));
        }

        private HttpRequestOptions Options(string url, CancellationToken cancellationToken)
        {
            var options = new HttpRequestOptions
            {
                Url = url,
                CancellationToken = cancellationToken,
                AcceptHeader = "application/json, image/*",
                TimeoutMs = 30000,
                LogErrors = false,
            };
            var token = Plugin.Instance.Configuration.EffectiveToken();
            if (!string.IsNullOrEmpty(token))
            {
                options.RequestHeaders["Authorization"] = "Bearer " + token;
            }

            return options;
        }

        /// <summary>The series at this URL; <c>null</c> when fav has none there (404, 409).</summary>
        private async Task<FavSeries> Fetch(string url, CancellationToken cancellationToken)
        {
            if (Cache.TryGetValue(url, out var cached) && DateTime.UtcNow - cached.Item1 < CacheFor)
            {
                return cached.Item2;
            }

            var options = Options(url, cancellationToken);
            options.ThrowOnErrorResponse = false;
            using (var response = await _http.GetResponse(options).ConfigureAwait(false))
            {
                FavSeries series = null;
                if (response.StatusCode == HttpStatusCode.OK)
                {
                    series = await _json.DeserializeFromStreamAsync<FavSeries>(response.Content).ConfigureAwait(false);
                }
                else if (response.StatusCode != HttpStatusCode.NotFound && response.StatusCode != HttpStatusCode.Conflict)
                {
                    _logger.Warn("Huangguo: fav answered {0} for {1}", (int)response.StatusCode, url);
                    return null;
                }

                Cache[url] = Tuple.Create(DateTime.UtcNow, series);
                return series;
            }
        }
    }
}
