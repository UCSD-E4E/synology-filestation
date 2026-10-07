"""Tests for ``Client.md5`` / ``AsyncClient.md5``: the NAS-side MD5 task.

DSM computes the digest itself (``SYNO.FileStation.MD5`` v2): ``start`` returns
a task id, ``status`` is polled until ``finished`` with the hex digest. Every
call goes to ``/webapi/entry.cgi``, so a single handler dispatches on
``method`` and records what it saw.

The core polls once a second, so each ``finished: false`` costs about 1 s.
"""
from __future__ import annotations

import json

import pytest
from werkzeug import Request, Response

from synology_filestation import Client, NoSuchFile, PermissionDenied
from synology_filestation.aio import AsyncClient

DIGEST = "9e107d9d372bb6826bd81d3542a419d6"


class FakeMd5Task:
    """Serves the MD5 start/status/stop protocol; one unfinished poll first."""

    def __init__(self, *, unfinished_polls: int = 1, start_error: int | None = None):
        self.unfinished_polls = unfinished_polls
        self.start_error = start_error
        self.calls: list[dict[str, str]] = []

    def __call__(self, request: Request) -> Response:
        args = dict(request.values)
        if args.get("api") != "SYNO.FileStation.MD5":
            # Anything else on entry.cgi (login's share listing) is not under
            # test; answer it the way an empty NAS would.
            body = {"success": True, "data": {"shares": [], "files": []}}
            return Response(json.dumps(body), content_type="application/json")
        self.calls.append(args)
        method = args.get("method")
        if method == "start":
            if self.start_error is not None:
                body = {"success": False, "error": {"code": self.start_error}}
            else:
                body = {"success": True, "data": {"taskid": "MD5-task-1"}}
        elif method == "status":
            assert args.get("taskid") == "MD5-task-1"
            if self.unfinished_polls > 0:
                self.unfinished_polls -= 1
                body = {"success": True, "data": {"finished": False}}
            else:
                body = {"success": True, "data": {"finished": True, "md5": DIGEST}}
        elif method == "stop":
            body = {"success": True}
        else:  # pragma: no cover - a request the protocol does not have
            body = {"success": False, "error": {"code": 101}}
        return Response(json.dumps(body), content_type="application/json")

    def methods(self) -> list[str]:
        return [c.get("method", "") for c in self.calls]


def _serve(httpserver, task: FakeMd5Task) -> None:
    httpserver.expect_request("/webapi/auth.cgi").respond_with_json(
        {"success": True, "data": {"sid": "md5-sid"}}
    )
    httpserver.expect_request("/webapi/entry.cgi").respond_with_handler(task)


def _sync_client(host_port) -> Client:
    host, port = host_port
    return Client.login(host, port, "alice", "secret", https=False, auto_relogin=False)


class TestSyncMd5:
    def test_returns_digest_after_polling(self, httpserver, host_port):
        task = FakeMd5Task(unfinished_polls=1)
        _serve(httpserver, task)
        c = _sync_client(host_port)

        assert c.md5("/share/photos/img.orf") == DIGEST
        assert task.methods() == ["start", "status", "status"]
        assert task.calls[0]["file_path"] == "/share/photos/img.orf"

    def test_missing_file_raises_no_such_file(self, httpserver, host_port):
        task = FakeMd5Task(start_error=414)
        _serve(httpserver, task)
        c = _sync_client(host_port)

        with pytest.raises(NoSuchFile) as exc:
            c.md5("/share/missing.bin")
        assert exc.value.code == 414
        assert task.methods() == ["start"]

    def test_no_permission_raises_permission_denied(self, httpserver, host_port):
        # A top-level 408 maps the same way it does for every other call
        # (only getinfo's per-entry 408 is remapped to NoSuchFile).
        task = FakeMd5Task(start_error=408)
        _serve(httpserver, task)
        c = _sync_client(host_port)

        with pytest.raises(PermissionDenied) as exc:
            c.md5("/share/locked.bin")
        assert exc.value.code == 408


@pytest.mark.asyncio
class TestAsyncMd5:
    async def test_returns_digest_after_polling(self, httpserver, host_port):
        task = FakeMd5Task(unfinished_polls=1)
        _serve(httpserver, task)
        host, port = host_port
        c = await AsyncClient.login(
            host, port, "alice", "secret", https=False, auto_relogin=False
        )

        assert await c.md5("/share/photos/img.orf") == DIGEST
        assert task.methods() == ["start", "status", "status"]
        assert task.calls[0]["file_path"] == "/share/photos/img.orf"

    async def test_missing_file_raises_no_such_file(self, httpserver, host_port):
        task = FakeMd5Task(start_error=414)
        _serve(httpserver, task)
        host, port = host_port
        c = await AsyncClient.login(
            host, port, "alice", "secret", https=False, auto_relogin=False
        )

        with pytest.raises(NoSuchFile) as exc:
            await c.md5("/share/missing.bin")
        assert exc.value.code == 414
