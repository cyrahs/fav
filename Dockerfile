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

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PLAYWRIGHT_BROWSERS_PATH=/ms-playwright \
    PATH="/app/.venv/bin:${PATH}"

WORKDIR /app

# Runtime dependencies including yt-dlp and ffmpeg (required for audio/video merge)
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
       -o /usr/local/bin/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp

# Bring in the virtualenv from the builder image
COPY --from=builder /app/.venv /app/.venv

# Install the Playwright-managed Chromium build and its distro libraries, then
# fail the image build if headless Chromium cannot launch.
RUN python -m playwright install --with-deps chromium \
    && rm -rf /var/lib/apt/lists/* \
    && python -c "from playwright.sync_api import sync_playwright; pw = sync_playwright().start(); browser = pw.chromium.launch(headless=True); page = browser.new_page(); page.set_content('<main>ok</main>'); assert page.text_content('main') == 'ok'; browser.close(); pw.stop()"

# Copy the application source
COPY src/ src/
COPY script/ script/
COPY run.py ./

# Compile application code to bytecode
RUN python -m compileall src/ script/ run.py

CMD ["python", "-m", "src.service"]
