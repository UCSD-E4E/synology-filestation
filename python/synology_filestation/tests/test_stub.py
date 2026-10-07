"""The type stub ships, and says what the compiled module actually has.

``py.typed`` declares the package typed, and a type checker reads
``_native.pyi`` rather than the compiled module. Without a stub every name
from ``_native`` was ``Any`` — ``domain=`` and ``transport`` included — and a
stub that falls behind the module is worse than none. This keeps them in step.
"""
from __future__ import annotations

import ast
from pathlib import Path

import pytest

import synology_filestation
from synology_filestation import _native

STUB = Path(synology_filestation.__file__).with_name("_native.pyi")


def _stub_classes() -> dict[str, ast.ClassDef]:
    tree = ast.parse(STUB.read_text())
    return {n.name: n for n in tree.body if isinstance(n, ast.ClassDef)}


def _members(cls: ast.ClassDef) -> dict[str, ast.AST]:
    return {
        n.name: n
        for n in cls.body
        if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))
    }


def test_the_stub_ships_beside_the_module():
    assert STUB.is_file()
    assert Path(synology_filestation.__file__).with_name("py.typed").is_file()


@pytest.mark.parametrize("name", ["Client", "AsyncClient"])
def test_every_public_member_is_declared(name):
    runtime = {
        a
        for a in dir(getattr(_native, name))
        if not a.startswith("_") or a in ("__enter__", "__exit__")
    }
    declared = set(_members(_stub_classes()[name]))
    assert runtime - declared == set(), "missing from _native.pyi"
    assert declared - runtime - {"__init__"} == set(), "not in the module"


@pytest.mark.parametrize("name", ["Client", "AsyncClient"])
def test_login_takes_a_keyword_only_domain(name):
    login = _members(_stub_classes()[name])["login"]
    assert "domain" in [a.arg for a in login.args.kwonlyargs]


def test_every_exception_is_declared():
    exported = {a for a in dir(_native) if isinstance(getattr(_native, a), type)}
    assert exported - set(_stub_classes()) == set()


def test_version_is_the_wheel_version():
    # It was a literal, "0.1.16", that nothing bumped after 0.1.16.
    from importlib.metadata import version

    assert synology_filestation.__version__ == version("synology-filestation")
