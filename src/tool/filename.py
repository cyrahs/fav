"""Utilities for sanitizing and formatting filenames."""

import re
from pathlib import Path

# Invalid characters for filenames across different OS
INVALID_CHARS = r'[<>:"/\\|?*\n]'


def sanitize(name: str, max_bytes: int = 200) -> str:
    """
    Sanitize a string to be safe for use as a filename.
    
    Args:
        name: The string to sanitize
        max_bytes: Maximum bytes for the filename (default: 200)
    
    Returns:
        Sanitized string safe for filenames
    """
    base = re.sub(INVALID_CHARS, '_', name).strip()
    while base and len(base.encode('utf-8')) > max_bytes:
        base = base[:-1]
    return base


def format_video_filename(
    title: str,
    video_id: str,
    uploader: str | None = None,
    ext: str = 'mp4',
    max_total_bytes: int = 240,
) -> str:
    """
    Format a video filename with uploader, title, and video ID.
    
    The format is: [uploader]title [video_id].ext
    If uploader is None, the format is: title [video_id].ext
    
    The total filename is limited to max_total_bytes. Components are sanitized with these limits:
    - video_id: max 100 bytes
    - uploader: max 100 bytes
    - title: adjusted to fit within total limit
    
    Args:
        title: Video title
        video_id: Video ID (e.g., BV number for Bilibili, message ID for Telegram)
        uploader: Uploader name (optional)
        ext: File extension without dot (default: 'mp4')
        max_total_bytes: Maximum bytes for the entire filename (default: 240)
    
    Returns:
        Formatted filename string
    """
    # Sanitize video_id and uploader with their limits
    video_id = sanitize(video_id, max_bytes=100)
    video_id_bytes = len(video_id.encode('utf-8'))
    
    # Ensure extension doesn't have a leading dot
    if ext.startswith('.'):
        ext = ext[1:]
    
    # Calculate remaining bytes for title
    # Structural characters '[', ']', ' ', '.', ext are already accounted for in max_total_bytes
    if uploader:
        uploader = sanitize(uploader, max_bytes=100)
        uploader_bytes = len(uploader.encode('utf-8'))
        max_title_bytes = max_total_bytes - video_id_bytes - uploader_bytes
    else:
        max_title_bytes = max_total_bytes - video_id_bytes
    
    # Sanitize and trim title to fit
    title = sanitize(title, max_bytes=max_title_bytes)
    
    # Build final filename
    if uploader:
        filename = f'[{uploader}]{title} [{video_id}]'
    else:
        filename = f'{title} [{video_id}]'
    
    return f'{filename}.{ext}'


def ensure_unique_path(path: Path) -> Path:
    """
    Ensure the path is unique by appending a counter if it already exists.
    
    Args:
        path: The original path
    
    Returns:
        A unique path that doesn't exist
    """
    if not path.exists():
        return path
    
    counter = 1
    while True:
        new_path = path.with_stem(f'{path.stem} ({counter})')
        if not new_path.exists():
            return new_path
        counter += 1

