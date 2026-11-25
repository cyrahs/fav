# Huangguo Emby plugin

An Emby plugin (`Fav.Emby.Huangguo.dll`) that gives the Huangguo TV library its metadata and images
from fav: the series' title, plot, tags (as genres), author (as studio), score, premiere date and
status, its poster, and each episode's title, running time and thumbnail. fav saves all of it when
it downloads the series (README, "What the Emby plugin reads"); the plugin reads it from fav's
`/media/huangguo` routes, so nothing is written into the strm tree and nothing goes to 115.

It registers, all named **Huangguo**:

- a metadata provider for series: it finds the series by its folder
  (`/media/ai/短剧/<series>` → `dir_name=<series>&category=短剧`) the first time and by the
  `Huangguo` provider id it stores after;
- a metadata provider for episodes: the series' saved episode with the episode's number
  (`<series> S01E04.strm` → 4);
- an image provider: the series poster and the episode thumbnails, as `Primary` images. Their URLs
  are the signed paths fav gives (`poster_url`, `thumbnail_url`), because Emby fetches a provider's
  image URL by itself, without the token (`/Images/Remote`, and images it keeps as URLs);
- an external id linking a series to its page on huangguoai.com.

## Build

```bash
dotnet build emby-plugin/Fav.Emby.Huangguo/Fav.Emby.Huangguo.csproj -c Release
```

It targets `netstandard2.0` against the `MediaBrowser.Server.Core` package. CI
(`.github/workflows/emby-plugin.yml`) builds `emby-plugin/Dockerfile` whenever this folder changes
and pushes `ghcr.io/cyrahs/fav-emby-huangguo:latest`: an image that holds only the DLL. Its command
copies the DLL into `/config/plugins`, which is what Emby's initContainer runs in the cluster.

## Setup

1. fav: give Emby a client with the `huangguo-media` scope in `CREDENTIAL_CLIENTS`
   (`emby:<sha256 of the token>:huangguo-media`). Only the hash goes into fav's configuration.
2. Emby: the token (and, outside the cluster, fav's address) on the plugin's settings page, or
   in the environment as `FAV_HUANGGUO_TOKEN` and `FAV_HUANGGUO_URL`. The address defaults to
   `http://fav.media.svc.cluster.local`.
3. In the Huangguo library's settings, tick **Huangguo** under the series and episode metadata
   downloaders and image fetchers (and move it first). The plugin answers nothing for a folder fav
   does not know, so it does no harm in other libraries, but there is no reason to enable it there.
4. Refresh the library's metadata. Images stored before the URLs were signed (fav answers 401 to
   Emby for them) need a refresh with **Replace existing images** ticked.
