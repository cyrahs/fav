FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim AS builder

WORKDIR /app

# Tools needed only during build to fetch yt-dlp
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy project metadata first for better layer caching of deps
COPY pyproject.toml uv.lock ./

# Install Python deps using uv into a local .venv
RUN uv sync --no-dev --frozen

# Pre-fetch yt-dlp binary to the expected path (./bin/yt-dlp)
RUN mkdir -p /app/bin \
    && curl -fsSL \
    https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
    -o /app/bin/yt-dlp \
    && chmod +x /app/bin/yt-dlp


FROM python:3.12-slim-bookworm AS runner

ENV PYTHONUNBUFFERED=1 \
    PATH="/app/.venv/bin:${PATH}"

WORKDIR /app

# Minimal runtime dependencies
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ffmpeg \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Bring in the virtualenv and yt-dlp from the builder image
COPY --from=builder /app/.venv /app/.venv
COPY --from=builder /app/bin /app/bin

# Copy the application source
COPY src/ src/
COPY run.py ./

CMD ["python", "main.py"]
