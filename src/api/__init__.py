"""fav API backend package."""

from .app import create_app
from .server import main

__all__ = ['create_app', 'main']
