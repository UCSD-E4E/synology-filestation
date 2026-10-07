"""Type stub for the compiled ``synology_filestation._native`` module.

Kept in step with the module by ``tests/test_stub.py``.
"""
from __future__ import annotations

from types import TracebackType
from typing import Literal, TypedDict

Transport = Literal["smb", "http"]
TransportReason = Literal[
    "disabled", "unreachable", "auth_refused", "auth_cooldown", "disconnected"
]

class FileInfo(TypedDict):
    name: str
    path: str
    isdir: bool
    size: int | None
    mtime: int | None
    atime: int | None
    ctime: int | None
    perm: int | None

class Client:
    @staticmethod
    def login(
        host: str,
        port: int,
        username: str,
        password: str,
        *,
        https: bool = True,
        verify_ssl: bool = True,
        otp: str | None = None,
        auto_relogin: bool = True,
        throttle: bool = True,
        max_concurrency: int = 4,
        min_interval_ms: int = 150,
        max_attempts: int = 5,
        backoff_base_ms: int = 1000,
        backoff_max_ms: int = 60000,
        domain: str | None = None,
    ) -> Client:
        """Log in over HTTP(S), then prefer SMB with the same credentials.

        ``domain`` names the SMB (AD) domain. ``None`` takes it from
        ``DOMAIN\\\\user`` or ``user@realm`` in ``username``, else from
        ``SYNOLOGY_FS_SMB_DOMAIN``; ``""`` means a local DSM account.
        """
    @property
    def transport(self) -> Transport:
        """``"smb"`` or ``"http"``: which transport file operations prefer."""
    @property
    def transport_reason(self) -> TransportReason | None:
        """Why the client is on HTTP, or ``None`` on SMB."""
    @property
    def transport_detail(self) -> str | None:
        """The fallback in words, including what to change; ``None`` on SMB."""
    def logout(self) -> None: ...
    def exists(self, path: str) -> bool: ...
    def getinfo(self, path: str) -> FileInfo: ...
    def download(self, path: str, *, offset: int = 0, length: int = 0) -> bytes: ...
    def md5(self, path: str) -> str: ...
    def list_dir(self, path: str) -> list[FileInfo]: ...
    def list_shares(self) -> list[FileInfo]: ...
    def upload_bytes(self, remote_path: str, data: bytes, *, overwrite: bool = True) -> None: ...
    def download_to(self, remote_path: str, local_path: str) -> None: ...
    def upload(
        self,
        local_path: str,
        remote_dir: str,
        *,
        overwrite: bool = True,
        _create_parents: bool = True,
    ) -> None: ...
    def create_folder(self, parent_dir: str, name: str, *, _force_parent: bool = True) -> None: ...
    def delete(self, remote_path: str) -> None: ...
    def __enter__(self) -> Client: ...
    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_value: BaseException | None,
        traceback: TracebackType | None,
    ) -> bool: ...

class AsyncClient:
    @staticmethod
    async def login(
        host: str,
        port: int,
        username: str,
        password: str,
        *,
        https: bool = True,
        verify_ssl: bool = True,
        otp: str | None = None,
        auto_relogin: bool = True,
        throttle: bool = True,
        max_concurrency: int = 4,
        min_interval_ms: int = 150,
        max_attempts: int = 5,
        backoff_base_ms: int = 1000,
        backoff_max_ms: int = 60000,
        domain: str | None = None,
    ) -> AsyncClient:
        """As :meth:`Client.login`."""
    @property
    def transport(self) -> Transport: ...
    @property
    def transport_reason(self) -> TransportReason | None: ...
    @property
    def transport_detail(self) -> str | None: ...
    async def logout(self) -> None: ...
    async def exists(self, path: str) -> bool: ...
    async def getinfo(self, path: str) -> FileInfo: ...
    async def download(self, path: str, *, offset: int = 0, length: int = 0) -> bytes: ...
    async def md5(self, path: str) -> str: ...
    async def list_dir(self, path: str) -> list[FileInfo]: ...
    async def list_shares(self) -> list[FileInfo]: ...
    async def upload_bytes(
        self, remote_path: str, data: bytes, *, overwrite: bool = True
    ) -> None: ...
    async def download_to(self, remote_path: str, local_path: str) -> None: ...
    async def upload(
        self,
        local_path: str,
        remote_dir: str,
        *,
        overwrite: bool = True,
        _create_parents: bool = True,
    ) -> None: ...
    async def upload_file(
        self, local_path: str, remote_path: str, *, overwrite: bool = True
    ) -> None: ...
    async def create_folder(
        self, parent_dir: str, name: str, *, _force_parent: bool = True
    ) -> None: ...
    async def delete(self, remote_path: str) -> None: ...

class FileStationError(Exception):
    code: int | None
    message: str

class AuthError(FileStationError): ...
class SidNotFound(AuthError): ...
class NoSuchFile(FileStationError): ...
class PermissionDenied(FileStationError): ...
class AlreadyExists(FileStationError): ...
class NoSpace(FileStationError): ...
class NotEmpty(FileStationError): ...
class InvalidArg(FileStationError): ...
class NotSupported(FileStationError): ...
class TransportError(FileStationError): ...
class TlsError(TransportError): ...
class DSMError(FileStationError): ...
