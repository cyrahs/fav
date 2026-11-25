using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using MediaBrowser.Common.Net;
using MediaBrowser.Controller.Entities;
using MediaBrowser.Controller.Entities.TV;
using MediaBrowser.Controller.Providers;
using MediaBrowser.Model.Configuration;
using MediaBrowser.Model.Entities;
using MediaBrowser.Model.Logging;
using MediaBrowser.Model.Providers;
using MediaBrowser.Model.Serialization;

namespace Fav.Emby.Huangguo
{
    internal static class Convert
    {
        public static DateTimeOffset? Date(string text)
        {
            return DateTimeOffset.TryParseExact(
                text,
                "yyyy-MM-dd",
                CultureInfo.InvariantCulture,
                DateTimeStyles.AssumeUniversal,
                out var date)
                ? date
                : (DateTimeOffset?)null;
        }

        public static Series Series(FavSeries found)
        {
            var series = new Series
            {
                Name = found.title,
                Overview = found.plot,
                CommunityRating = found.score.HasValue ? (float?)found.score.Value : null,
                PremiereDate = Date(found.premiere_date),
                Status = found.finished ? SeriesStatus.Ended : SeriesStatus.Continuing,
                Genres = found.tags ?? Array.Empty<string>(),
                Tags = string.IsNullOrEmpty(found.category) ? Array.Empty<string>() : new[] { found.category },
                Studios = string.IsNullOrEmpty(found.author) ? Array.Empty<string>() : new[] { found.author },
            };
            series.ProductionYear = series.PremiereDate?.Year;
            series.SetProviderId(Plugin.ProviderKey, found.id.ToString(CultureInfo.InvariantCulture));
            return series;
        }

        public static RemoteSearchResult SearchResult(FavSeries found)
        {
            var result = new RemoteSearchResult
            {
                Name = found.title,
                Overview = found.plot,
                PremiereDate = Date(found.premiere_date),
                SearchProviderName = Plugin.ProviderName,
                ImageUrl = found.has_poster ? FavApi.PosterUrl(found) : null,
            };
            result.ProductionYear = result.PremiereDate?.Year;
            result.SetProviderId(Plugin.ProviderKey, found.id.ToString(CultureInfo.InvariantCulture));
            return result;
        }

        /// <summary>The folder of the series an episode file is in.</summary>
        public static string SeriesFolder(string episodePath)
        {
            return string.IsNullOrEmpty(episodePath) ? null : Path.GetDirectoryName(episodePath);
        }
    }

    /// <summary>The series' details, by its folder the first time, by its id after.</summary>
    public class HuangguoSeriesProvider : IRemoteMetadataProvider<Series, SeriesInfo>, IHasOrder
    {
        private readonly FavApi _fav;

        public HuangguoSeriesProvider(IHttpClient http, IJsonSerializer json, ILogManager logManager)
        {
            _fav = new FavApi(http, json, logManager.GetLogger(Plugin.ProviderName));
        }

        public string Name => Plugin.ProviderName;

        public int Order => 0;

        public async Task<MetadataResult<Series>> GetMetadata(SeriesInfo info, CancellationToken cancellationToken)
        {
            var found = await _fav.Find(info.GetProviderId(Plugin.ProviderKey), info.Path, cancellationToken).ConfigureAwait(false);
            if (found == null)
            {
                return new MetadataResult<Series>();
            }

            return new MetadataResult<Series> { HasMetadata = true, Item = Convert.Series(found) };
        }

        public async Task<IEnumerable<RemoteSearchResult>> GetSearchResults(SeriesInfo searchInfo, CancellationToken cancellationToken)
        {
            var found = await _fav.Find(searchInfo.GetProviderId(Plugin.ProviderKey), searchInfo.Path, cancellationToken).ConfigureAwait(false);
            return found == null ? Array.Empty<RemoteSearchResult>() : new[] { Convert.SearchResult(found) };
        }

        public Task<HttpResponseInfo> GetImageResponse(string url, CancellationToken cancellationToken)
        {
            return _fav.Image(url, cancellationToken);
        }
    }

    /// <summary>An episode's title and running time.</summary>
    public class HuangguoEpisodeProvider : IRemoteMetadataProvider<Episode, EpisodeInfo>, IHasOrder
    {
        private readonly FavApi _fav;

        public HuangguoEpisodeProvider(IHttpClient http, IJsonSerializer json, ILogManager logManager)
        {
            _fav = new FavApi(http, json, logManager.GetLogger(Plugin.ProviderName));
        }

        public string Name => Plugin.ProviderName;

        public int Order => 0;

        public async Task<MetadataResult<Episode>> GetMetadata(EpisodeInfo info, CancellationToken cancellationToken)
        {
            var result = new MetadataResult<Episode>();
            if (!info.IndexNumber.HasValue)
            {
                return result;
            }

            info.SeriesProviderIds.TryGetValue(Plugin.ProviderKey, out var seriesId);
            var found = await _fav.Find(seriesId, Convert.SeriesFolder(info.Path), cancellationToken).ConfigureAwait(false);
            var episode = found?.episodes?.FirstOrDefault(item => item.ep_num == info.IndexNumber.Value);
            if (episode == null)
            {
                return result;
            }

            result.HasMetadata = true;
            result.Item = new Episode
            {
                Name = string.IsNullOrEmpty(episode.title) ? $"第{episode.ep_num}集" : episode.title,
                IndexNumber = episode.ep_num,
                ParentIndexNumber = 1,
                RunTimeTicks = episode.duration > 0 ? TimeSpan.FromSeconds(episode.duration).Ticks : (long?)null,
            };
            return result;
        }

        public Task<IEnumerable<RemoteSearchResult>> GetSearchResults(EpisodeInfo searchInfo, CancellationToken cancellationToken)
        {
            return Task.FromResult<IEnumerable<RemoteSearchResult>>(Array.Empty<RemoteSearchResult>());
        }

        public Task<HttpResponseInfo> GetImageResponse(string url, CancellationToken cancellationToken)
        {
            return _fav.Image(url, cancellationToken);
        }
    }

    /// <summary>The series poster and each episode's thumbnail, as fav saved them.</summary>
    public class HuangguoImageProvider : IRemoteImageProvider, IHasOrder
    {
        private readonly FavApi _fav;

        public HuangguoImageProvider(IHttpClient http, IJsonSerializer json, ILogManager logManager)
        {
            _fav = new FavApi(http, json, logManager.GetLogger(Plugin.ProviderName));
        }

        public string Name => Plugin.ProviderName;

        public int Order => 0;

        public bool Supports(BaseItem item) => item is Series || item is Episode;

        public IEnumerable<ImageType> GetSupportedImages(BaseItem item) => new[] { ImageType.Primary };

        public async Task<IEnumerable<RemoteImageInfo>> GetImages(BaseItem item, LibraryOptions libraryOptions, CancellationToken cancellationToken)
        {
            var images = new List<RemoteImageInfo>();
            if (item is Series series)
            {
                var found = await _fav.Find(series.GetProviderId(Plugin.ProviderKey), series.Path, cancellationToken).ConfigureAwait(false);
                if (found != null && found.has_poster)
                {
                    images.Add(Image(FavApi.PosterUrl(found)));
                }
            }
            else if (item is Episode episode && episode.IndexNumber.HasValue)
            {
                var found = await _fav.Find(episode.Series?.GetProviderId(Plugin.ProviderKey), Convert.SeriesFolder(episode.Path), cancellationToken)
                    .ConfigureAwait(false);
                var number = episode.IndexNumber.Value;
                var saved = found?.episodes?.FirstOrDefault(candidate => candidate.ep_num == number && candidate.has_thumbnail);
                if (saved != null)
                {
                    images.Add(Image(FavApi.ThumbnailUrl(found, saved)));
                }
            }

            return images;
        }

        public Task<HttpResponseInfo> GetImageResponse(string url, CancellationToken cancellationToken)
        {
            return _fav.Image(url, cancellationToken);
        }

        private static RemoteImageInfo Image(string url)
        {
            return new RemoteImageInfo { ProviderName = Plugin.ProviderName, Url = url, Type = ImageType.Primary };
        }
    }

    /// <summary>The site's page of a matched series.</summary>
    public class HuangguoExternalId : IExternalId
    {
        public string Name => Plugin.ProviderName;

        public string Key => Plugin.ProviderKey;

        public string UrlFormatString => "https://huangguoai.com/video/{0}/";

        public bool Supports(IHasProviderIds item) => item is Series;
    }
}
