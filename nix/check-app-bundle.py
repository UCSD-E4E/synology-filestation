#!/usr/bin/env python3
"""Assert that the darwin `synologyfuse-gui` output carries a launchable .app.

Run as: check-app-bundle.py <package-out-path> <expected-version>

Everything here is a property that `nix build` cannot fail on by itself: the
derivation will happily install a bundle that macOS refuses to launch, or one
that launches without an icon, and the only symptom is a user reporting that
nothing shows up in Spotlight. The two subtle ones:

* `Contents/MacOS` has to hold the whole payload and the executable the
  launcher hands off to. macOS derives `NSBundle.mainBundle` from the path of
  the *running* executable, so a launcher that `exec`s a store path outside the
  bundle leaves the process with no Info.plist at all — no `CFBundleIconFile`,
  no `CFBundleDisplayName`, a generic Dock tile. That failure looks like a
  cosmetic glitch and is actually a broken bundle.

* The mount is an AppleScript `mount volume` sent to Finder, so the bundle
  needs `NSAppleEventsUsageDescription`. Without it macOS denies the Apple
  event outright (errAEEventNotPermitted) instead of prompting, and the GUI
  fails to mount only when launched from the Dock — never from a terminal,
  where the terminal's own automation grant covers it.
"""

import os
import plistlib
import shlex
import sys

out, expected_version = sys.argv[1], sys.argv[2]

app = os.path.join(out, "Applications", "SynologyFuse.app")
contents = os.path.join(app, "Contents")
macos = os.path.join(contents, "MacOS")
resources = os.path.join(contents, "Resources")

failures: list[str] = []


def check(condition: bool, message: str) -> bool:
    if not condition:
        failures.append(message)
    return condition


if check(os.path.isdir(app), f"no .app bundle at {app}"):
    plist_path = os.path.join(contents, "Info.plist")

    if check(os.path.isfile(plist_path), f"no Info.plist at {plist_path}"):
        with open(plist_path, "rb") as handle:
            try:
                plist = plistlib.load(handle)
            except Exception as exc:  # noqa: BLE001 — report, don't traceback
                plist = {}
                failures.append(f"Info.plist does not parse: {exc}")

        # A missed substitution leaves the template's own placeholder behind,
        # which macOS reads as a literal version string.
        version = plist.get("CFBundleShortVersionString")
        check(
            version == expected_version,
            f"CFBundleShortVersionString is {version!r}, expected {expected_version!r}",
        )

        check(
            bool(plist.get("NSAppleEventsUsageDescription")),
            "Info.plist has no NSAppleEventsUsageDescription, so the Finder "
            "mount will be denied without a prompt",
        )

        executable = plist.get("CFBundleExecutable")
        launcher = os.path.join(macos, executable) if executable else None
        if check(
            bool(launcher) and os.path.isfile(launcher),
            f"CFBundleExecutable {executable!r} is not a file in Contents/MacOS",
        ):
            check(
                os.access(launcher, os.X_OK),
                f"{executable} is not executable",
            )

            # The launcher is a makeWrapper script; every absolute path it
            # hands off to must stay inside the bundle.
            with open(launcher, encoding="utf-8", errors="replace") as handle:
                script = handle.read()

            check(
                "SYNOFS_NATIVE_DIR" in script,
                "the launcher does not set SYNOFS_NATIVE_DIR, so the GUI will "
                "start and then fail to find the FFI library",
            )

            exec_lines = [line for line in script.splitlines() if line.startswith("exec ")]
            if check(bool(exec_lines), "the launcher has no exec line"):
                # shlex, because makeWrapper quotes the target path.
                target = next(
                    (
                        word
                        for word in shlex.split(exec_lines[-1])
                        if word.startswith("/nix/store/")
                    ),
                    None,
                )
                if check(
                    target is not None,
                    f"cannot find the exec target in: {exec_lines[-1]}",
                ):
                    check(
                        os.path.dirname(os.path.realpath(target)) == os.path.realpath(macos),
                        f"the launcher execs {target}, which is outside "
                        "Contents/MacOS — the process would have no main bundle",
                    )
                    check(
                        os.path.isfile(target),
                        f"the launcher's exec target {target} does not exist",
                    )

        icon = plist.get("CFBundleIconFile")
        icns = os.path.join(resources, f"{icon}.icns") if icon else None
        if check(
            bool(icns) and os.path.isfile(icns),
            f"CFBundleIconFile {icon!r} has no .icns in Contents/Resources",
        ):
            check(os.path.getsize(icns) > 0, f"{icns} is empty")

    # The apphost resolves its managed assembly from its own directory.
    check(
        os.path.isfile(os.path.join(macos, "SynologyFuse.Gui.dll")),
        "SynologyFuse.Gui.dll is not beside the apphost in Contents/MacOS",
    )

# `nix run .#gui`, `meta.mainProgram` and a bare `SynologyFuse.Gui` on PATH all
# go through $out/bin. It must reach the same launcher, or the two entry points
# drift and only one of them is ever tested.
cli = os.path.join(out, "bin", "SynologyFuse.Gui")
if check(os.path.exists(cli), f"no {cli}"):
    check(
        os.path.realpath(cli).startswith(os.path.realpath(app)),
        f"{cli} resolves to {os.path.realpath(cli)}, outside the .app bundle",
    )

if failures:
    print("app bundle check failed:", file=sys.stderr)
    for failure in failures:
        print(f"  - {failure}", file=sys.stderr)
    sys.exit(1)

print(f"app bundle OK: {app}")
