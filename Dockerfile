# DEPRECATED: This Dockerfile is retained for reference only and is no longer used in the workflow.
# Flashpoint Mock Mempool - Dockerfile
# Build a minimal image running uvicorn

FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

COPY . .

EXPOSE 8000
# Optional: set an API key at runtime with -e FPN_API_KEY=...
# HEALTHCHECK could be added to hit /health

CMD ["uvicorn", "mempool.app:app", "--host", "0.0.0.0", "--port", "8000"]
