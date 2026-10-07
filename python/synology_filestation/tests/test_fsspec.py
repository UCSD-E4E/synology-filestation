"""Tests for the fsspec backend (``synology_filestation.fsspec``).

Pin down the contract of the ``synofs`` fsspec protocol:

* Auto-discovery via ``fsspec.filesystem("synofs", ...)`` works after
  ``pip install synology-filestation[fsspec]``.
* Sync API: ``ls``, ``info``, ``cat``, ``exists``, ``get``, ``put``,
  ``pipe``, ``rm``, ``mkdir`` all dispatch to the underlying client.
* Async API: ``_ls``, ``_cat_file``, ``_get_file`` are awaitable.
* **The fishsense regression**: ``fs.get(remote, local)`` inherits the
  atomic-rename semantics of ``download_to`` — a DSM error must not leave
  a zero-byte file at the destination.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest


def _ensure_fsspec():
    """Skip the test if fsspec isn't installed (matches the optional-extra contract)."""
    pytest.importorskip("fsspec")


@pytest.fixture
def synofs(httpserver, host_port):
    """A SynologyFileSystem pointed at the local mock server, pre-authed."""
    _ensure_fsspec()
    from synology_filestation.fsspec import SynologyFileSystem

    httpserver.expect_request("/webapi/auth.cgi").respond_with_json(
        {"success": True, "data": {"sid": "fsspec-sid"}}
    )
    host, port = host_port
    fs = SynologyFileSystem(
        host=host,
        port=port,
        username="alice",
        password="secret",
        https=False,
        auto_relogin=False,
        skip_instance_cache=True,
    )
    yield fs


# ─── Sync API ───────────────────────────────────────────────────────────────


class TestSyncBasic:
    def test_protocol_is_synofs(self):
        _ensure_fsspec()
        from synology_filestation.fsspec import SynologyFileSystem

        # Class attribute. fsspec uses it for URL routing.
        assert SynologyFileSystem.protocol == "synofs"

    def test_filesystem_factory_resolves_synofs_protocol(self, httpserver, host_port):
        """`fsspec.filesystem("synofs", ...)` must work via the entry point."""
        _ensure_fsspec()
        import fsspec

        httpserver.expect_request("/webapi/auth.cgi").respond_with_json(
            {"success": True, "data": {"sid": "factory-sid"}}
        )
        host, port = host_port
        fs = fsspec.filesystem(
            "synofs",
            host=host,
            port=port,
            username="alice",
            password="secret",
            https=False,
            auto_relogin=False,
            skip_instance_cache=True,
        )
        from synology_filestation.fsspec import SynologyFileSystem

        assert isinstance(fs, SynologyFileSystem)


class TestSyncLs:
    def test_ls_returns_full_paths_with_detail_false(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "a.txt",
                            "path": "/share/a.txt",
                            "isdir": False,
                            "additional": {"size": 12},
                        },
                        {
                            "name": "b",
                            "path": "/share/b",
                            "isdir": True,
                            "additional": None,
                        },
                    ]
                },
            }
        )
        names = synofs.ls("/share", detail=False)
        assert sorted(names) == ["/share/a.txt", "/share/b"]

    def test_ls_returns_dicts_with_fsspec_keys(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "a.txt",
                            "path": "/share/a.txt",
                            "isdir": False,
                            "additional": {"size": 100},
                        }
                    ]
                },
            }
        )
        entries = synofs.ls("/share", detail=True)
        assert len(entries) == 1
        # fsspec convention: name is full path, type is "file" or "directory"
        assert entries[0]["name"] == "/share/a.txt"
        assert entries[0]["type"] == "file"
        assert entries[0]["size"] == 100

    def test_ls_root_lists_shares(self, httpserver, host_port, synofs):
        # FileStation has no real root; "/" is the shares list. The only
        # request made by ls("/") is the list_share call, so we can mock
        # /webapi/entry.cgi without further query-string restrictions.
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "shares": [
                        {"name": "photos", "path": "/photos", "isdir": True, "additional": None},
                        {"name": "docs", "path": "/docs", "isdir": True, "additional": None},
                    ]
                },
            }
        )
        entries = synofs.ls("/", detail=True)
        names = [e["name"] for e in entries]
        assert "/photos" in names
        assert all(e["type"] == "directory" for e in entries)


class TestSyncInfo:
    def test_info_returns_fsspec_dict(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "report.pdf",
                            "path": "/share/report.pdf",
                            "isdir": False,
                            "additional": {"size": 4096},
                        }
                    ]
                },
            }
        )
        info = synofs.info("/share/report.pdf")
        assert info["name"] == "/share/report.pdf"
        assert info["size"] == 4096
        assert info["type"] == "file"

    def test_info_missing_path_raises_file_not_found(
        self, httpserver, host_port, synofs
    ):
        # fsspec convention: missing path → FileNotFoundError, not our
        # internal NoSuchFile. This lets generic fsspec-compatible code
        # catch the standard exception.
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {"files": [{"code": 408, "path": "/share/missing"}]},
            }
        )
        with pytest.raises(FileNotFoundError):
            synofs.info("/share/missing")


class TestSyncCat:
    def test_cat_returns_bytes(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_data(
            b"cat-payload",
            content_type="application/octet-stream",
        )
        assert synofs.cat("/share/file.bin") == b"cat-payload"


class TestSyncExists:
    def test_returns_true_for_existing_path(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "x",
                            "path": "/share/x",
                            "isdir": False,
                            "additional": {"size": 1},
                        }
                    ]
                },
            }
        )
        assert synofs.exists("/share/x") is True

    def test_returns_false_for_missing(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {"success": True, "data": {"files": [{"code": 408, "path": "/share/missing"}]}}
        )
        assert synofs.exists("/share/missing") is False


class TestSyncGetPut:
    def test_get_uses_atomic_download(self, httpserver, host_port, synofs, tmp_path):
        """The fishsense regression test, expressed via fsspec.

        ``fs.get(remote, local)`` must use ``download_to`` semantics — on a
        DSM error response, the destination must not exist on disk. This
        is the headline value-add of using this package over ``synology-api``
        + fsspec's HTTP backend.
        """
        httpserver.expect_request("/webapi/entry.cgi").respond_with_data(
            json.dumps({"success": False, "error": {"code": 119}}),
            content_type="application/json",
        )
        dest = tmp_path / "must_not_exist.bin"
        from synology_filestation import SidNotFound

        with pytest.raises(SidNotFound):
            synofs.get("/share/file.bin", str(dest))

        assert not dest.exists()
        assert not (tmp_path / "must_not_exist.bin.part").exists()

    def test_get_happy_path(self, httpserver, host_port, synofs, tmp_path):
        # fsspec's high-level `get` calls `_isdir(rpath)` (which calls
        # `info()` → DSM getinfo) before dispatching to `_get_file`. So
        # two server roundtrips: getinfo first, then download.
        #
        # Login also settles how the session id travels (cookie vs `_sid`) with
        # one probe call to list_share, and that lands on entry.cgi before
        # either of them. Oneshot handlers are consumed in registration order,
        # so the probe response has to be registered first or it would eat the
        # getinfo response below.
        httpserver.expect_oneshot_request("/webapi/entry.cgi").respond_with_json(
            {"success": True, "data": {"total": 0, "shares": []}}
        )
        httpserver.expect_oneshot_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "file.bin",
                            "path": "/share/file.bin",
                            "isdir": False,
                            "additional": {"size": 18},
                        }
                    ]
                },
            }
        )
        httpserver.expect_request("/webapi/entry.cgi").respond_with_data(
            b"fsspec-get-payload",
            content_type="application/octet-stream",
        )
        dest = tmp_path / "out.bin"
        synofs.get("/share/file.bin", str(dest))
        assert dest.read_bytes() == b"fsspec-get-payload"


class TestSyncRm:
    def test_rm_calls_delete(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {"success": True}
        )
        synofs.rm("/share/old.txt")  # should not raise


class TestSyncMkdir:
    def test_mkdir_creates_folder(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "folders": [
                        {
                            "name": "newdir",
                            "path": "/share/newdir",
                            "isdir": True,
                            "additional": None,
                        }
                    ]
                },
            }
        )
        synofs.mkdir("/share/newdir")  # should not raise


# ─── Async API ──────────────────────────────────────────────────────────────


class TestAsync:
    pytestmark = pytest.mark.asyncio

    async def test_async_cat_returns_bytes(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_data(
            b"async-cat",
            content_type="application/octet-stream",
        )
        # AsyncFileSystem exposes async methods with leading underscore.
        result = await synofs._cat_file("/share/file.bin")
        assert result == b"async-cat"

    async def test_async_ls_returns_dicts(self, httpserver, host_port, synofs):
        httpserver.expect_request("/webapi/entry.cgi").respond_with_json(
            {
                "success": True,
                "data": {
                    "files": [
                        {
                            "name": "f",
                            "path": "/share/f",
                            "isdir": False,
                            "additional": {"size": 7},
                        }
                    ]
                },
            }
        )
        entries = await synofs._ls("/share", detail=True)
        assert entries[0]["name"] == "/share/f"
        assert entries[0]["size"] == 7
        assert entries[0]["type"] == "file"

    async def test_async_get_atomic_no_zero_byte_on_dsm_error(
        self, httpserver, host_port, synofs, tmp_path
    ):
        """Async equivalent of the fishsense regression test."""
        httpserver.expect_request("/webapi/entry.cgi").respond_with_data(
            json.dumps({"success": False, "error": {"code": 119}}),
            content_type="application/json",
        )
        dest = tmp_path / "async_dest.bin"
        from synology_filestation import SidNotFound

        with pytest.raises(SidNotFound):
            await synofs._get_file("/share/file.bin", str(dest))

        assert not dest.exists()
        assert not (tmp_path / "async_dest.bin.part").exists()


# ─── Which account SMB uses ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_domain_reaches_the_smb_probe(httpserver, host_port, smb_unreachable):
    # A filesystem built from storage_options has no other way to name the
    # SMB domain than the process-wide environment variable.
    _ensure_fsspec()
    from synology_filestation.fsspec import SynologyFileSystem

    httpserver.expect_request("/webapi/auth.cgi").respond_with_json(
        {"success": True, "data": {"sid": "fsspec-sid"}}
    )
    host, port = host_port
    fs = SynologyFileSystem(
        host=host,
        port=port,
        username="svc_fishsense",
        password="secret",
        https=False,
        domain="KRG",
        asynchronous=True,
        skip_instance_cache=True,
    )
    client = await fs._get_client()
    assert client.transport == "http"
    assert "KRG\\svc_fishsense" in client.transport_detail
