"""Which transport a client is on, and why.

Every login probes SMB with the login's credentials and falls back to HTTP. The
fallback used to be invisible, which hid a misconfigured username for weeks
while each failed probe counted towards DSM's auto-block. These tests pin the
surface that makes the choice visible (``transport``, ``transport_reason``,
``transport_detail``), the explicit ``domain=`` keyword, and that the Rust
core's warnings reach Python's ``logging`` at all.

A refused SMB login cannot be produced here: it needs a server that completes
SMB negotiation. The refusal memory is covered by the smb crate's tests.
"""
from __future__ import annotations

import json
import logging

import pytest
from werkzeug import Request, Response

from synology_filestation import Client
from synology_filestation.aio import AsyncClient


def _serve_login(httpserver) -> None:
    httpserver.expect_request("/webapi/auth.cgi").respond_with_json(
        {"success": True, "data": {"sid": "transport-sid"}}
    )


def _login(host_port, username="alice", **kw) -> Client:
    host, port = host_port
    return Client.login(host, port, username, "secret", https=False, **kw)


class TestTransport:
    def test_disabled_smb_says_so(self, httpserver, host_port):
        # conftest sets SYNOLOGY_FS_SMB_DISABLE for the whole suite.
        _serve_login(httpserver)
        c = _login(host_port)

        assert c.transport == "http"
        assert c.transport_reason == "disabled"
        assert "SYNOLOGY_FS_SMB_DISABLE" in c.transport_detail

    def test_unreachable_smb_falls_back_without_a_warning(
        self, httpserver, host_port, smb_unreachable, caplog
    ):
        _serve_login(httpserver)
        with caplog.at_level(logging.DEBUG):
            c = _login(host_port)

        assert c.transport == "http"
        assert c.transport_reason == "unreachable"
        # A network failure costs no strike, so it stays quiet, as before.
        assert not [r for r in caplog.records if r.levelno >= logging.WARNING]

    def test_domain_keyword_names_the_smb_account(
        self, httpserver, host_port, smb_unreachable
    ):
        _serve_login(httpserver)
        c = _login(host_port, "svc_fishsense", domain="KRG")
        assert "KRG\\svc_fishsense" in c.transport_detail

    def test_domain_keyword_wins_over_the_environment(
        self, httpserver, host_port, smb_unreachable, monkeypatch
    ):
        monkeypatch.setenv("SYNOLOGY_FS_SMB_DOMAIN", "ENVDOM")
        _serve_login(httpserver)
        c = _login(host_port, "svc_fishsense", domain="KRG")
        assert "KRG\\svc_fishsense" in c.transport_detail

    def test_without_the_keyword_the_environment_then_the_username_decide(
        self, httpserver, host_port, smb_unreachable, monkeypatch
    ):
        _serve_login(httpserver)
        assert "KRG\\bob" in _login(host_port, "KRG\\bob").transport_detail
        monkeypatch.setenv("SYNOLOGY_FS_SMB_DOMAIN", "ENVDOM")
        assert "ENVDOM\\carol" in _login(host_port, "carol").transport_detail

    def test_domain_is_keyword_only(self, httpserver, host_port):
        _serve_login(httpserver)
        host, port = host_port
        with pytest.raises(TypeError):
            Client.login(host, port, "alice", "secret", "KRG")  # type: ignore[misc]


@pytest.mark.asyncio
class TestAsyncTransport:
    async def test_reports_the_fallback(self, httpserver, host_port, smb_unreachable):
        _serve_login(httpserver)
        host, port = host_port
        c = await AsyncClient.login(
            host, port, "svc_fishsense", "secret", https=False, domain="KRG"
        )
        assert c.transport == "http"
        assert c.transport_reason == "unreachable"
        assert "KRG\\svc_fishsense" in c.transport_detail


def test_rust_warnings_reach_python_logging(httpserver, host_port, caplog):
    """The probe's refusal warning is only useful if Python can see it.

    Driven through a warning that can be produced without SMB: an MD5 task
    that can be neither read nor stopped.
    """
    def handler(request: Request) -> Response:
        method = request.values.get("method")
        if method == "start":
            body = {"success": True, "data": {"taskid": "FileStation_1"}}
        else:
            body = {"success": False, "error": {"code": 599}}
        return Response(json.dumps(body), content_type="application/json")

    _serve_login(httpserver)
    httpserver.expect_request("/webapi/entry.cgi").respond_with_handler(handler)
    c = _login(host_port, auto_relogin=False)

    with caplog.at_level(logging.WARNING):
        with pytest.raises(Exception):
            c.md5("/share/x.bin")

    assert any(
        r.levelno == logging.WARNING and "could not stop" in r.getMessage()
        for r in caplog.records
    ), [r.getMessage() for r in caplog.records]
