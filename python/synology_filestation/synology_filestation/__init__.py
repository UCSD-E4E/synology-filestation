"""Synology FileStation client for Python.

A drop-in replacement for the ``synology-api`` PyPI package's FileStation
surface with three concrete behavior fixes:

1. DSM JSON-error responses raise typed exceptions instead of returning
   success-shaped empty data.
2. ``download_to`` writes atomically (temp file + rename) so a failed
   download never leaves a 0-byte stub at the destination.
3. ``Client.login(..., auto_relogin=True)`` (the default) transparently
   re-authenticates on SID-expired errors and retries the operation once.

See the project README for the exception hierarchy and usage examples.
"""

from ._native import (  # noqa: F401 — re-exported for users
    Client,
    FileStationError,
    AuthError,
    SidNotFound,
    NoSuchFile,
    PermissionDenied,
    AlreadyExists,
    NoSpace,
    NotEmpty,
    InvalidArg,
    NotSupported,
    TransportError,
    TlsError,
    DSMError,
)

try:
    from importlib.metadata import version as _version

    __version__ = _version("synology-filestation")
except Exception:  # pragma: no cover - not installed as a distribution
    __version__ = "0+unknown"

__all__ = [
    "Client",
    "FileStationError",
    "AuthError",
    "SidNotFound",
    "NoSuchFile",
    "PermissionDenied",
    "AlreadyExists",
    "NoSpace",
    "NotEmpty",
    "InvalidArg",
    "NotSupported",
    "TransportError",
    "TlsError",
    "DSMError",
]
