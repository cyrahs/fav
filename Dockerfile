FROM ghcr.io/astral-sh/uv:python3.12-bookworm-slim AS builder

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


FROM python:3.12-slim-bookworm AS runner

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    PATH="/app/.venv/bin:${PATH}"

WORKDIR /app

# Runtime dependencies including yt-dlp
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ffmpeg \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
       -o /usr/local/bin/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp

# Bring in the virtualenv from the builder image
COPY --from=builder /app/.venv /app/.venv

# Copy the application source
COPY src/ src/
COPY run.py ./

# Compile application code to bytecode
RUN python -m compileall -b src/ run.py && \
    find . -name "*.py" -not -path "./.venv/*" -delete && \
    find . -name "*.pyc" -exec rename 's/\.pyc$/.py/' {} \;

CMD ["python", "run.py"]
