from pathlib import Path

from src.core import config, logger

log = logger.get('azurlane')
cfg = config.web.azurlane


class AzurLane:
    def __init__(self, *, path: Path | None = None) -> None:
        self.path = Path(path or cfg.path)

    async def update(self) -> None:
        log.info('Azur Lane crawler is not implemented yet; skipping update')
