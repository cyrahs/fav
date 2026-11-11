# The fav image. Build from the repository root:
#
#   docker build .
#
# `.github/workflows/ci.yml` publishes it from main as `latest` (and semver tags from
# `v*` tags) after the checks pass; Flux deploys `latest` (charys117/nas-gitops apps/fav).
#
# Besides the binary, the image carries what fav shells out to: yt-dlp and gallery-dl at
# the versions `docker/downloaders/uv.lock` pins (Dependabot bumps them there), ffmpeg,
# the Playwright-managed Chromium build at Playwright's registry path, TigerVNC's Xvnc
# for the login window, and tini as PID 1.

FROM node:22-slim AS web-builder

WORKDIR /web

# The React front end.
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build


FROM rust:1.97-slim-trixie AS rust-builder

WORKDIR /src

# The slim image has gcc but not make, which the jemalloc build (the binary's global
# allocator, chosen by the WP18 memory benchmark) needs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends make \
    && rm -rf /var/lib/apt/lists/*

# The toolchain, the lockfile and the crates' manifests and sources, then every crate
# downloaded in its own layer (rebuilt when any of them change).
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo fetch --locked

RUN cargo build --release --locked -p fav \
    && install -D -m 0755 target/release/fav /out/fav


FROM ghcr.io/astral-sh/uv:python3.12-trixie-slim AS downloaders

WORKDIR /lock

# yt-dlp and gallery-dl at the versions `docker/downloaders/uv.lock` pins, exported as
# constraints; the requirement strings (yt-dlp with its `[default]` extra) are the ones
# its `pyproject.toml` declares. Playwright is installed alongside only to fetch its
# Chromium build below, then dropped.
COPY docker/downloaders/pyproject.toml docker/downloaders/uv.lock ./
RUN uv export --frozen --no-dev --no-emit-project --no-hashes --format requirements-txt -o constraints.txt \
    && uv venv /opt/downloaders \
    && uv pip install --python /opt/downloaders/bin/python --constraint constraints.txt \
        'yt-dlp[default]' gallery-dl \
    && uv venv /opt/playwright \
    && uv pip install --python /opt/playwright/bin/python --constraint constraints.txt playwright


FROM python:3.12-slim-trixie AS runner
ARG TARGETARCH

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright \
    PATH="/opt/downloaders/bin:${PATH}"

WORKDIR /app

# ffmpeg merges the separate audio and video streams Bilibili serves (neither Python
# package bundles it). Xvnc (TigerVNC) is the virtual display a 登录态 login runs a
# headed Chromium on, started only for the length of one login. tini runs as PID 1 to
# reap Chromium processes that outlive the driver.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    tigervnc-standalone-server \
    tini \
    && rm -rf /var/lib/apt/lists/*

COPY --from=downloaders /opt/downloaders /opt/downloaders

# The Playwright-managed Chromium build (the revision `fav_browser::chromium` looks for
# under PLAYWRIGHT_BROWSERS_PATH) and its distro libraries, installed by Playwright's own
# installer from a throwaway environment that is removed again. The launch smoke runs
# only on native amd64 builds (Chromium's GPU process can crash
# under qemu).
COPY --from=downloaders /opt/playwright /opt/playwright
# The revision checked is the one `fav_browser::chromium` launches (CHROMIUM_REVISION; a test
# in crates/fav keeps it, the line below and uv.lock's Playwright in step).
RUN /opt/playwright/bin/python -m playwright install --with-deps chromium \
    && rm -rf /opt/playwright /var/lib/apt/lists/* \
    && chrome="/ms-playwright/chromium-1228/$(if [ "$TARGETARCH" = "arm64" ]; then echo chrome-linux; else echo chrome-linux64; fi)/chrome" \
    && test -x "$chrome" \
    && if [ "$TARGETARCH" = "amd64" ]; then \
        "$chrome" --headless --no-sandbox --disable-gpu --dump-dom 'data:text/html,<main>ok</main>' | grep -q '<main>ok</main>'; \
    fi

COPY --from=rust-builder /out/fav /usr/local/bin/fav

# The API serves this directory when it exists; without it the image is API-only.
COPY --from=web-builder /web/dist web/dist

# Everything the binary shells out to is on PATH, the Chromium it launches is where it
# looks, and the NIKKE layer-metadata tool is the binary's own `nikke-layers` command.
RUN fav --version \
    && fav nikke-layers --help >/dev/null \
    && yt-dlp --version \
    && gallery-dl --version \
    && ffmpeg -hide_banner -version >/dev/null \
    && command -v Xvnc >/dev/null \
    && command -v tini >/dev/null

# Reported by the MCP get_runtime tool. Last, because they change on every build.
ARG FAV_GIT_COMMIT=""
ARG FAV_BUILD_TIME=""
ENV FAV_GIT_COMMIT=${FAV_GIT_COMMIT} \
    FAV_BUILD_TIME=${FAV_BUILD_TIME}

ENTRYPOINT ["tini", "--"]
CMD ["fav", "serve"]
