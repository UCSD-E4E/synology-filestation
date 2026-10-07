"""fsspec backend for Synology FileStation.

Registered as protocol ``synofs``. Both the sync and async fsspec APIs are
exposed from a single :class:`SynologyFileSystem` (an
:class:`fsspec.asyn.AsyncFileSystem` subclass — sync methods are
auto-generated wrappers around the async implementations).

Example::

    import fsspec

    fs = fsspec.filesystem(
        "synofs",
        host="nas.example.com", port=5001,
        username="alice", password="secret",
    )

    # Sync — atomic download via download_to (the fishsense bug fix).
    fs.get("/share/img.orf", "/tmp/img.orf")

    # Async
    async def main():
        data = await fs._cat_file("/share/img.orf")

    # As pandas storage_options
    import pandas as pd
    pd.read_csv(
        "synofs://share/data.csv",
        storage_options={
            "host": "nas.example.com", "port": 5001,
            "username": "alice", "password": "secret",
        },
    )
"""
from __future__ import annotations

from typing import Any

# fsspec is an optional dependency: importing this module without
# `pip install synology-filestation[fsspec]` will fail at import time, which
# is what we want — the user asked for an fsspec-flavored API.
from fsspec.asyn import AsyncFileSystem
from fsspec.spec import AbstractBufferedFile

from . import AlreadyExists, NoSuchFile
from .aio import AsyncClient


class SynologyFileSystem(AsyncFileSystem):
    """fsspec backend for Synology FileStation.

    Construction args (also accepted as ``storage_options=`` kwargs):
      * ``host`` (str), ``port`` (int): DSM HTTPS endpoint
      * ``username`` (str), ``password`` (str)
      * ``https`` (bool, default True)
      * ``verify_ssl`` (bool, default True): verify the NAS TLS certificate.
        Set False only for a self-signed DSM certificate — the connection is
        then encrypted but not authenticated.
      * ``otp`` (str | None, default None): one-time TOTP code for 2FA
      * ``auto_relogin`` (bool, default True): transparent re-auth on SID
        expiry. Turn off only for 2FA accounts (where re-login can't reuse
        the original OTP).
      * ``domain`` (str | None, default None): the SMB (AD) domain, as for
        :meth:`AsyncClient.login`. Without it, a bare username is tried
        over SMB as a local NAS account.

    The underlying :class:`synology_filestation.aio.AsyncClient` is created
    lazily on the first I/O call so that constructing a FileSystem object
    doesn't open a network connection.
    """

    protocol = "synofs"

    def __init__(
        self,
        host: str,
        port: int,
        username: str,
        password: str,
        *,
        https: bool = True,
        verify_ssl: bool = True,
        otp: str | None = None,
        auto_relogin: bool = True,
        domain: str | None = None,
        asynchronous: bool = False,
        loop=None,
        **kwargs,
    ):
        super().__init__(asynchronous=asynchronous, loop=loop, **kwargs)
        self._host = host
        self._port = port
        self._username = username
        self._password = password
        self._https = https
        self._verify_ssl = verify_ssl
        self._otp = otp
        self._auto_relogin = auto_relogin
        self._domain = domain
        self._client: AsyncClient | None = None

    # ── helpers ────────────────────────────────────────────────────────

    @classmethod
    def _strip_protocol(cls, path: str) -> str:
        """Strip the ``synofs://`` prefix and ensure a leading slash.

        FileStation paths are always absolute (``/share/...``); fsspec lets
        users write ``synofs://share/file`` or just ``/share/file`` and
        we want both to resolve to the same DSM path.
        """
        if isinstance(path, list):
            return [cls._strip_protocol(p) for p in path]
        for prefix in ("synofs://", "synofs:"):
            if path.startswith(prefix):
                path = path[len(prefix):]
                break
        if not path.startswith("/"):
            path = "/" + path
        return path

    async def _get_client(self) -> AsyncClient:
        """Lazy-construct the underlying AsyncClient, logging in once.

        We can't login in ``__init__`` because that's sync code and our
        login is async; we'd have to spin a temporary event loop, which
        wouldn't share state with fsspec's loop.
        """
        if self._client is None:
            self._client = await AsyncClient.login(
                self._host,
                self._port,
                self._username,
                self._password,
                https=self._https,
                verify_ssl=self._verify_ssl,
                otp=self._otp,
                auto_relogin=self._auto_relogin,
                domain=self._domain,
            )
        return self._client

    @staticmethod
    def _info_to_fsspec(info: dict, fallback_path: str | None = None) -> dict[str, Any]:
        """Translate a getinfo/list_dir entry into fsspec's expected dict shape.

        fsspec consumers expect at minimum ``name`` (full path), ``size``
        (int, 0 for unknown), and ``type`` ("file" or "directory"). We
        also pass through the DSM time/perm fields so callers can inspect
        them when needed.
        """
        return {
            "name": info.get("path") or (fallback_path or ""),
            "size": info.get("size") or 0,
            "type": "directory" if info.get("isdir") else "file",
            "mtime": info.get("mtime"),
            "atime": info.get("atime"),
            "ctime": info.get("ctime"),
            "perm": info.get("perm"),
        }

    # ── async core ops ─────────────────────────────────────────────────

    async def _info(self, path: str, **kwargs) -> dict[str, Any]:
        path = self._strip_protocol(path)
        # FileStation has no real root metadata; synthesize one so
        # `fs.info("/")` doesn't 404.
        if path in ("", "/"):
            return {"name": "/", "size": 0, "type": "directory"}
        client = await self._get_client()
        try:
            info = await client.getinfo(path)
        except NoSuchFile as e:
            raise FileNotFoundError(path) from e
        return self._info_to_fsspec(info, fallback_path=path)

    async def _ls(self, path: str, detail: bool = True, **kwargs):
        path = self._strip_protocol(path)
        client = await self._get_client()
        # The "/" root is the share list (DSM has no parent of shares).
        if path in ("", "/"):
            entries = await client.list_shares()
        else:
            try:
                entries = await client.list_dir(path)
            except NoSuchFile as e:
                raise FileNotFoundError(path) from e
        out = [self._info_to_fsspec(e) for e in entries]
        if detail:
            return out
        return [e["name"] for e in out]

    async def _cat_file(self, path: str, start=None, end=None, **kwargs) -> bytes:
        path = self._strip_protocol(path)
        client = await self._get_client()
        try:
            if start is None and end is None:
                return await client.download(path)
            offset = start or 0
            length = (end - offset) if end is not None else 0
            return await client.download(path, offset=offset, length=length)
        except NoSuchFile as e:
            raise FileNotFoundError(path) from e

    async def _get_file(self, rpath: str, lpath: str, **kwargs) -> None:
        # The headline value-add over fsspec's HTTP backend: this calls
        # download_to_path which writes to <lpath>.part and renames, so a
        # DSM error never leaves a zero-byte file at lpath.
        rpath = self._strip_protocol(rpath)
        client = await self._get_client()
        try:
            await client.download_to(rpath, lpath)
        except NoSuchFile as e:
            raise FileNotFoundError(rpath) from e

    async def _put_file(self, lpath: str, rpath: str, **kwargs) -> None:
        rpath = self._strip_protocol(rpath)
        client = await self._get_client()
        # Streams the local file to the NAS (straight to SMB when available, so
        # multi-GB uploads aren't buffered in memory; HTTP fallback otherwise).
        await client.upload_file(lpath, rpath)

    async def _pipe_file(self, path: str, value: bytes, **kwargs) -> None:
        path = self._strip_protocol(path)
        client = await self._get_client()
        await client.upload_bytes(path, value)

    async def _rm_file(self, path: str, **kwargs) -> None:
        path = self._strip_protocol(path)
        client = await self._get_client()
        try:
            await client.delete(path)
        except NoSuchFile as e:
            raise FileNotFoundError(path) from e

    async def _makedirs(self, path: str, exist_ok: bool = False, **kwargs) -> None:
        path = self._strip_protocol(path)
        client = await self._get_client()
        parent, _, name = path.rstrip("/").rpartition("/")
        if not parent:
            parent = "/"
        if not name:
            return  # "/" itself — already exists
        try:
            await client.create_folder(parent, name)
        except AlreadyExists:
            if not exist_ok:
                raise FileExistsError(path)

    async def _mkdir(self, path: str, create_parents: bool = True, **kwargs) -> None:
        # FileStation's create_folder takes force_parent=True, so the
        # distinction between mkdir and makedirs is moot at the API level.
        await self._makedirs(path, exist_ok=False, **kwargs)

    async def _exists(self, path: str, **kwargs) -> bool:
        path = self._strip_protocol(path)
        if path in ("", "/"):
            return True
        client = await self._get_client()
        return await client.exists(path)

    # ── sync-only seam: file-like for `fs.open(...)` ──────────────────

    def _open(self, path, mode="rb", **kwargs):
        # AsyncFileSystem doesn't auto-generate an async `_open`; it returns
        # a sync file-like object that does its I/O via cat_file/pipe_file.
        path = self._strip_protocol(path)
        if "r" not in mode:
            raise NotImplementedError(
                "synofs only supports read mode via .open() — use put_file/pipe_file for writes"
            )
        return SynologyBufferedFile(self, path, mode, **kwargs)


class SynologyBufferedFile(AbstractBufferedFile):
    """Buffered read of a SynologyFileSystem file using ranged downloads.

    fsspec calls ``_fetch_range(start, end)`` to fill its buffer; we forward
    to the parent FileSystem's ``cat_file`` which uses the DSM Range header.
    """

    def _fetch_range(self, start: int, end: int) -> bytes:
        return self.fs.cat_file(self.path, start=start, end=end)

    def _initiate_upload(self):
        raise NotImplementedError("synofs file-like writes not supported")

    def _upload_chunk(self, final: bool = False):
        raise NotImplementedError("synofs file-like writes not supported")
