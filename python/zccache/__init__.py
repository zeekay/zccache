"""Python bindings for the zccache watcher, fingerprint, and daemon APIs."""

from __future__ import annotations

from zccache import client, downloader, fingerprint, ino, watcher
from zccache.client import SessionStartResult, SessionStats, ZcCacheClient
from zccache.downloader import (
    DownloadApi,
    DownloadDaemonStatus,
    DownloadHandle,
    DownloadStatus,
    FetchResult,
    FetchState,
)
from zccache.fingerprint import (
    Api,
    FingerprintCache,
    FingerprintDecision,
    FingerprintManager,
    FingerprintResult,
)
from zccache.ino import InoConvertResult, convert_ino
from zccache.watcher import (
    DebouncedFileWatcherProcess,
    FileChangeEvent,
    FileWatcher,
    FileWatcherProcess,
    watch_files,
)

__all__ = [
    "Api",
    "DebouncedFileWatcherProcess",
    "DownloadApi",
    "DownloadDaemonStatus",
    "DownloadHandle",
    "DownloadStatus",
    "FetchResult",
    "FetchState",
    "FileChangeEvent",
    "FileWatcher",
    "FileWatcherProcess",
    "FingerprintCache",
    "FingerprintDecision",
    "FingerprintManager",
    "FingerprintResult",
    "InoConvertResult",
    "SessionStartResult",
    "SessionStats",
    "ZcCacheClient",
    "client",
    "convert_ino",
    "downloader",
    "fingerprint",
    "ino",
    "watcher",
    "watch_files",
]
