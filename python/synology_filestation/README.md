# synology-filestation

Python bindings for the Synology FileStation HTTP client (Rust core via PyO3).

This package is a drop-in replacement for the `synology-api` PyPI package's
FileStation surface, designed to fix three concrete footguns:

1. **DSM JSON-error responses are typed exceptions, not silent successes.**
   When DSM returns `200 OK` with body `{"success":false,"error":{"code":119}}`,
   `download` raises `SidNotFound`, not bytes.

2. **Atomic downloads.** `download_to(remote, local)` writes to `<local>.part`
   first, fsyncs, then renames. The destination either contains the complete
   file or doesn't exist — no zero-byte stubs after a failed download.

3. **Transparent session-expiry recovery.** Long-running scripts can opt into
   auto-relogin: when the DSM SID expires (~30 min idle), the client
   re-authenticates and retries the operation once. The caller never sees
   `SidNotFound` unless the re-login itself fails.

4. **Built-in throttle so bulk transfers can't take the NAS down.** Downloads
   and uploads are capped to a small concurrency, spaced by a rate-limit belt,
   and retried with bounded jittered backoff. On by default — see
   [Throttling & reliability](#throttling--reliability).

## Installation

```bash
pip install synology-filestation
```

## Usage

```python
from synology_filestation import Client

with Client.login("nas.example.com", 5001, "alice", "secret") as nas:
    if nas.exists("/photos/2026"):
        info = nas.getinfo("/photos/2026")
        print(info["size"])
    nas.download_to("/photos/2026/img.orf", "/tmp/img.orf")
```

Async API:

```python
from synology_filestation.aio import AsyncClient

async with AsyncClient.login("nas.example.com", 5001, "alice", "secret") as nas:
    data = await nas.download("/photos/2026/img.orf")
```

## fsspec backend

`pip install synology-filestation[fsspec]` registers the `synofs` protocol so you can use FileStation with any fsspec-aware tool (pandas, dask, polars, pyarrow):

```python
import fsspec

fs = fsspec.filesystem(
    "synofs",
    host="nas.example.com", port=5001,
    username="alice", password="secret",
)

# Atomic get — inherits download_to's <local>.part + rename semantics, so a
# DSM error never leaves a 0-byte file at the destination.
fs.get("/photos/2026/img.orf", "/tmp/img.orf")

# Or as pandas storage_options
import pandas as pd
df = pd.read_csv(
    "synofs://share/data.csv",
    storage_options={
        "host": "nas.example.com", "port": 5001,
        "username": "alice", "password": "secret",
    },
)
```

Both sync and async fsspec APIs are supported — `fs._cat_file(path)` returns an awaitable; `fs.cat_file(path)` is the auto-generated sync wrapper.

## Verifying a file: NAS-side MD5

`Client.md5(path)` (and `await AsyncClient.md5(path)`) returns the file's MD5 as
lowercase hex, computed **by the NAS** through `SYNO.FileStation.MD5` — no bytes
are downloaded. Use it to prove a transfer landed intact.

It is not free:

- **Time grows with file size.** DSM reads the whole file from disk to hash it;
  the client polls the task once a second and gives up after 15 minutes. A
  multi-GB file takes minutes.
- **It counts against the throttle like a download.** On a throttled client
  (the default) each call holds one of the `max_concurrency` transfer slots for
  as long as the hash runs, and it always goes through FileStation, even when
  transfers themselves use SMB.

If a call fails or times out, the client tells DSM to stop the task rather than
leaving it running on the appliance. Errors raise the usual typed exceptions,
e.g. `NoSuchFile` for a missing path.

```python
import hashlib
from synology_filestation import Client

def local_md5(path: str) -> str:
    h = hashlib.md5()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()

with Client.login("nas.example.com", 5001, "alice", "secret") as nas:
    nas.download_to("/photos/2026/img.orf", "/tmp/img.orf")
    if nas.md5("/photos/2026/img.orf") != local_md5("/tmp/img.orf"):
        raise RuntimeError("download does not match the NAS copy")
```

The fsspec backend does not use this for `checksum()`/`ukey()`: fsspec treats
those as cheap metadata fingerprints and may call them per file, which would
turn every call into a full read of the file on the NAS.

## TLS certificates

The NAS certificate is verified by default. A DSM appliance ships with a
self-signed certificate, so a stock NAS will be rejected until you either
install its certificate in the system trust store or opt out explicitly:

```python
from synology_filestation import Client, TlsError

try:
    nas = Client.login("nas.example.com", 5001, "alice", "secret")
except TlsError:
    # Encrypted, but not authenticated: anything able to intercept the
    # connection can present its own certificate and read the password.
    nas = Client.login(
        "nas.example.com", 5001, "alice", "secret", verify_ssl=False
    )
```

`verify_ssl=False` is also accepted by `AsyncClient.login` and as an fsspec
`storage_options` key. `TlsError` subclasses `TransportError`, so existing
`except TransportError` handlers keep working.

Before this option existed the client accepted *any* certificate
unconditionally, which made `https=True` encryption without authentication.
Upgrading will surface `TlsError` on a self-signed NAS that previously
connected silently — that is the fix working, not a regression.

## Throttling & reliability

The FileStation Download API is proxied through `nginx → synoscgi`, a shared
per-request CGI backend for the whole appliance. It is sized for a handful of
large streams, **not** a task-per-file fan-out. Parallel downloads — not total
volume — are what saturate it, and an inner retry storm (the same file fetched
hundreds of times) turns a blip into an outage.

Every `Client` / `AsyncClient` therefore ships with a throttle, **enabled by
default**, that wraps `download`/`download_to`/`upload`:

| Lever | Default | What it does |
|---|---|---|
| `max_concurrency` | `4` | Global semaphore around all transfer calls. Single-digit — a few big streams, not one request per file. |
| `min_interval_ms` | `150` | Rate-limit belt: minimum spacing between request starts, even at full concurrency. |
| `max_attempts` | `5` | Hard per-file retry cap. Once exhausted the error is raised — no unbounded inner loop. |
| `backoff_base_ms` / `backoff_max_ms` | `1000` / `60000` | Full-jitter exponential backoff between attempts (1s → 60s). |

```python
# Defaults are conservative; tune per-workload or disable with throttle=False.
nas = Client.login(
    "nas.example.com", 5001, "alice", "secret",
    max_concurrency=3,      # ≈3–4 is the safe band for synoscgi
    min_interval_ms=200,
    max_attempts=5,
)
```

**Error classification.** The throttle distinguishes transient from permanent
failures so it never retries something a retry can't fix:

- **Transient → back off + bounded retry:** HTTP 502/503/504, HTTP 407 (the
  backend fail-closing), connection/read errors, and DSM 402 *system busy*
  (backed off *harder*).
- **Permanent → fail fast, no retry:** missing file / no permission / invalid
  argument and any other DSM code. Retrying these wastes the backend's
  attention exactly like a 502 storm.

### Using this under Temporal (or any outer retry policy)

This client caps retries at `max_attempts` **and then raises** — deliberately.
Do **not** wrap it in your own inner retry loop. Let the failure propagate out
of the activity and let Temporal's retry policy reschedule it with its own
(longer, jittered) backoff. Two nested retry loops are exactly what produced the
200–250×-per-file storm that saturated the appliance. One activity ≈ one file;
bound the work here, reschedule out there.

## Which transport a client uses: SMB first, then HTTP

`login` also tries an in-process SMB connection with the same credentials, and
file transfers prefer it when it works, which keeps bulk traffic off
FileStation. If SMB can't be reached, the client falls back to HTTP without a
warning, as before. To see which transport a client got, and why:

```python
c = Client.login(host, 6021, "svc_fishsense", pw, domain="KRG")
c.transport          # "smb" or "http"
c.transport_reason   # None on SMB; else "disabled", "unreachable",
                     # "auth_refused", "auth_cooldown" or "disconnected"
c.transport_detail   # the same in words, including what to change
```

**The SMB login must name the domain.** FileStation accepts a bare
`svc_fishsense`, but SMB treats a bare name as a *local* NAS account, and the
login fails. Pass `domain="KRG"` (keyword-only, on `Client.login` and
`AsyncClient.login`), write the username as `KRG\\svc_fishsense`, or set
`SYNOLOGY_FS_SMB_DOMAIN`, in that order of precedence.

**A refused SMB login is remembered.** DSM counts each failed SMB login
toward its auto-block (on e4e-nas, 3 failures in 24 hours, and the block is
permanent), and the block applies to the caller's IP address for every
service on the NAS, HTTPS included. So after a refusal, the library:

- logs one `WARNING` (logger `synology_filestation_smb.probe`). For a bare
  username, the warning says the domain is the likely cause;
- uses HTTP for that (host, username, domain) for the next
  `SYNOLOGY_FS_SMB_AUTH_COOLDOWN_S` seconds (default 86400, DSM's window),
  across every `Client`/`AsyncClient` in the process. It does not dial SMB
  for that account again during the cool-down. Clients created in a burst
  wait for the first one's answer, so a burst spends at most one strike.

The memory is per process, so N worker processes can still spend N strikes
between them. Fix the account name instead of relying on the cool-down. Set
`SYNOLOGY_FS_SMB_DISABLE=1` to turn SMB off entirely.

## Bulk staging: prefer SMB/NFS over the Download API

For sustained bulk transfer of large binaries (e.g. staging raw `.ORF` frames),
the **structural fix is to not use the HTTP Download API at all**. It is an
interactive file-browser endpoint; streaming gigabytes through `synoscgi` is
outside what it is built for.

If your host can mount the share over **SMB or NFS** (the UCSD campus firewall
already permits both), read the bytes straight off the mounted share — that
bypasses `synoscgi` entirely, so there is no CGI backend to saturate. Treat this
package's HTTP Download path as the **fallback**, not the default, for bulk raw
staging. The throttle above is the safety net for when the API path must be
used; SMB/NFS is how you avoid needing it.

## Exceptions

```
FileStationError                  # base
├── AuthError                     # login failed
│   └── SidNotFound               # SID expired / not found (DSM code 119)
├── NoSuchFile                    # codes 403, 414, 415
├── PermissionDenied              # codes 408, 1805
├── AlreadyExists                 # codes 418, 1101
├── NoSpace                       # codes 419, 1804
├── NotEmpty                      # code 416
├── InvalidArg                    # code 400
├── NotSupported                  # operation not supported
├── TransportError                # network or parse error
└── DSMError                      # any unmapped DSM code; `.code` is set
```

All exceptions carry `.code` (the DSM error code, or `None`) and `.message`.
