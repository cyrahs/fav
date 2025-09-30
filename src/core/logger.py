"""Color logging with tqdm progress bar."""

from __future__ import annotations

import logging
from datetime import datetime
from pathlib import Path
from typing import Any

import colorlog
from tqdm import tqdm

NOTICE = 25
logging.addLevelName(NOTICE, 'NOTICE')


class MyLogger(logging.Logger):
    """Custom logger class."""

    def __init__(self, name: str, level: int = logging.NOTSET) -> None:
        """Initialize the logger."""
        super().__init__(name, level)

    def notice(self, msg: str, *args: Any, **kwargs: Any) -> None:
        """Log a message with level 'NOTICE'."""
        self.log(NOTICE, msg, *args, **kwargs)


logging.setLoggerClass(MyLogger)


class TqdmLoggingHandler(logging.Handler):
    """Handler for logging with tqdm progress bar."""

    def __init__(self) -> None:
        """Initialize the handler."""
        super().__init__()

    def emit(self, record: logging.LogRecord) -> None:
        """Emit the record."""
        msg = self.format(record)
        tqdm.write(msg)
        self.flush()


console_handler = TqdmLoggingHandler()
log_colors = colorlog.default_log_colors
log_colors['NOTICE'] = 'cyan'
console_formatter = colorlog.ColoredFormatter(
    '%(log_color)s[%(name)s]%(message)s',
    log_colors=log_colors,
)
console_handler.setFormatter(console_formatter)
root = logging.getLogger()
root.addHandler(console_handler)
app_logger = logging.getLogger('embyx')


log_dir = Path('./data/log')
log_dir.mkdir(exist_ok=True)

timestamp = datetime.now().astimezone().strftime('%Y%m%d')
file_handler = logging.FileHandler(log_dir / f'{timestamp}.log')
file_formatter = logging.Formatter('%(asctime)s - %(name)s - %(levelname)s - %(message)s')
file_handler.setFormatter(file_formatter)
root.addHandler(file_handler)

app_logger.setLevel(logging.INFO)


def get(name: str) -> MyLogger:
    """Get a child logger with the specified name.

    Args:
        name (str): name of the logger

    Returns:
        MyLogger: logger instance

    """
    return app_logger.getChild(name)  # type: ignore  # noqa: PGH003
