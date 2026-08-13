FROM node:22-slim AS web-builder

WORKDIR /web

# Install front-end deps first so a source-only change reuses the cached layer.
COPY web/package.json web/package-lock.json ./
RUN npm ci

COPY web/ ./
RUN npm run build


FROM ghcr.io/astral-sh/uv:python3.12-trixie-slim AS builder

WORKDIR /app

# Tools needed during build
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy project metadata first for better layer caching of deps
COPY pyproject.toml uv.lock ./

# Install Python deps using uv into a local .venv and compile to bytecode
RUN uv sync --no-dev --frozen --compile-bytecode


FROM python:3.12-slim-trixie AS runner
ARG TARGETARCH

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright \
    PATH="/app/.venv/bin:${PATH}"

WORKDIR /app

# ffmpeg merges the separate audio and video streams Bilibili serves. yt-dlp and
# gallery-dl are not installed here: both are uv dependencies and arrive with the
# virtualenv below, which puts their console scripts on PATH.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Bring in the virtualenv from the builder image
COPY --from=builder /app/.venv /app/.venv

# Install the Playwright-managed Chromium build and its distro libraries. The
# launch smoke runs only on native amd64 builds; arm64 is built through qemu in
# CI, where Chromium's GPU process can crash before runtime.
RUN python -m playwright install --with-deps chromium \
    && rm -rf /var/lib/apt/lists/* \
    && if [ "$TARGETARCH" = "amd64" ]; then \
        python -c "from playwright.sync_api import sync_playwright; pw = sync_playwright().start(); browser = pw.chromium.launch(headless=True); page = browser.new_page(); page.set_content('<main>ok</main>'); assert page.text_content('main') == 'ok'; browser.close(); pw.stop()"; \
    else \
        python -c "from playwright.sync_api import sync_playwright; sync_playwright().start().stop()"; \
    fi

# Copy the application source
COPY src/ src/
COPY script/ script/
COPY run.py ./

# The API serves this directory when it exists; without it the image is API-only.
COPY --from=web-builder /web/dist web/dist

# Compile application code to bytecode
RUN python -m compileall src/ script/ run.py

CMD ["python", "-m", "src.service"]
