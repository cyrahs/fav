# The Huangguo Emby plugin as an image whose only content is the plugin's DLL. Emby's
# initContainer (nas-gitops apps/emby/emby) copies it into /config/plugins before Emby
# starts. Built and pushed by .github/workflows/emby-plugin.yml.
FROM mcr.microsoft.com/dotnet/sdk:8.0 AS build
WORKDIR /src
COPY Fav.Emby.Huangguo/Fav.Emby.Huangguo.csproj Fav.Emby.Huangguo/
RUN dotnet restore Fav.Emby.Huangguo/Fav.Emby.Huangguo.csproj
COPY Fav.Emby.Huangguo/ Fav.Emby.Huangguo/
RUN dotnet build Fav.Emby.Huangguo/Fav.Emby.Huangguo.csproj -c Release --no-restore -o /out

FROM busybox:1.37
COPY --from=build /out/Fav.Emby.Huangguo.dll /plugin/Fav.Emby.Huangguo.dll
# What the initContainer runs, with Emby's config volume at /config.
CMD ["sh", "-c", "mkdir -p /config/plugins && cp /plugin/Fav.Emby.Huangguo.dll /config/plugins/ && ls -l /config/plugins/Fav.Emby.Huangguo.dll"]
