"""Pytest fixtures shared by the binding tests.

The fixtures use ``pytest_httpserver`` to stand up a tiny real HTTP server on a
random port and point a real ``synology_filestation.Client`` at it. We can't
mock the HTTP layer in Python because the requests are issued from Rust
(reqwest); the only seam is the network itself.
"""
from __future__ import annotations

import json
import os
from typing import Any, Iterator

import pytest
from pytest_httpserver import HTTPServer

# The client now transparently prefers SMB when reachable. These tests exercise
# the HTTP layer against a local mock server (no SMB), so disable the SMB probe
# for determinism and speed — SMB behavior is covered by the Rust crate's tests.
os.environ["SYNOLOGY_FS_SMB_DISABLE"] = "1"


# Stock responses the tests assemble together. Keeping them in one place
# makes it obvious which DSM error shapes we're modelling.

LOGIN_OK = {"success": True, "data": {"sid": "test-sid-123"}}
LOGIN_OK_FRESH = {"success": True, "data": {"sid": "fresh-sid-xyz"}}
LOGIN_BAD_PASSWORD = {"success": False, "error": {"code": 400}}

DSM_119 = {"success": False, "error": {"code": 119}}  # SID not found / expired
DSM_408 = {"success": False, "error": {"code": 408}}  # No permission
DSM_414 = {"success": False, "error": {"code": 414}}  # No such file
DSM_418 = {"success": False, "error": {"code": 418}}  # Already exists


@pytest.fixture
def host_port(httpserver: HTTPServer) -> tuple[str, int]:
    """The host/port the local pytest-httpserver is bound to."""
    return httpserver.host, httpserver.port


@pytest.fixture
def client(httpserver: HTTPServer, host_port: tuple[str, int]):
    """A logged-in Client pointed at the local HTTP server.

    Default: auto_relogin=False so that 119s surface to the test directly.
    Tests that need auto-relogin construct their own Client.
    """
    from synology_filestation import Client

    httpserver.expect_request("/webapi/auth.cgi").respond_with_json(LOGIN_OK)

    host, port = host_port
    c = Client.login(host, port, "alice", "secret", https=False, auto_relogin=False)
    yield c
    httpserver.clear()


def respond_dsm_json(httpserver: HTTPServer, uri: str, body: dict[str, Any]) -> None:
    """Register a single DSM-shaped JSON response on `uri`."""
    httpserver.expect_request(uri).respond_with_json(body)


def respond_dsm_json_then(
    httpserver: HTTPServer, uri: str, first: dict[str, Any], then: dict[str, Any]
) -> None:
    """Register two responses on the same URI: the first request gets `first`,
    every subsequent request gets `then`. Used to model SID expiry → recovery."""
    # ``expect_oneshot_request`` matches once and is then dequeued; a regular
    # ``expect_request`` after it acts as the fall-through. pytest-httpserver
    # honors the registration order.
    httpserver.expect_oneshot_request(uri).respond_with_json(first)
    httpserver.expect_request(uri).respond_with_json(then)


def respond_bytes(httpserver: HTTPServer, uri: str, payload: bytes) -> None:
    """Register a binary (non-JSON) response so download() returns bytes."""
    httpserver.expect_request(uri).respond_with_data(
        payload, content_type="application/octet-stream"
    )


def respond_dsm_error_then_bytes(
    httpserver: HTTPServer, uri: str, code: int, payload: bytes
) -> None:
    """First request → 200 + DSM JSON error envelope (the fishsense bug shape).
    Subsequent requests → real bytes."""
    httpserver.expect_oneshot_request(uri).respond_with_data(
        json.dumps({"success": False, "error": {"code": code}}),
        content_type="application/json",
    )
    httpserver.expect_request(uri).respond_with_data(
        payload, content_type="application/octet-stream"
    )


def _closed_port() -> int:
    """A local port nothing listens on, so an SMB dial is refused at once."""
    import socket

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture
def smb_unreachable(monkeypatch):
    """Let the SMB probe run, against a port that refuses the connection."""
    monkeypatch.delenv("SYNOLOGY_FS_SMB_DISABLE", raising=False)
    monkeypatch.delenv("SYNOLOGY_FS_SMB_DOMAIN", raising=False)
    monkeypatch.setenv("SYNOLOGY_FS_SMB_PORT", str(_closed_port()))
    monkeypatch.setenv("SYNOLOGY_FS_SMB_TIMEOUT_MS", "1000")
