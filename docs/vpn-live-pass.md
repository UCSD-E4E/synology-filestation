# Validating SMB over the OpenVPN tunnel

Everything this driver is being pointed at assumes one thing that has never
been observed: that an off-campus client can bring up the NAS's own OpenVPN
tunnel and mount SMB through it. The SMB backend has met mocks and a Docker
Samba container; it has not met DSM. Until this page has been walked through
once, treat the tunnel work as unvalidated.

The client that does this without `sudo` is now built, so this page has two
halves: step 3 proves SMB works over the NAS's tunnel at all, and step 4 proves
our own client can be that tunnel. Running them in that order is what makes a
failure in the second one mean something.

It needs a human, off campus, with AD credentials. It is the last open item on
the server side too — `spec/e4e-nas/vpn.yml` in `KastnerRG/krg-infra` lists it
as bring-up step 7, the only one not yet done.

## What is already true

Steps 1–6 of that spec are applied: the VPN Server package is installed and
configured, local accounts are denied VPN access, and as of 2026-08-19 both
firewall rules are open, so `udp/1194` answers from the internet. The server is
OpenVPN 2.5.11 with `tls-auth` on, `AES-256-CBC` / `SHA512`, no compression,
and no DNS push. Clients get an address on `10.90.24.0/24` and can reach
exactly one thing: `10.90.24.1:445`.

## 1. Get the profile

`e4e-nas-vpn.ovpn` lives in the `installers` share, readable by any AD user.
It embeds `ta.key`, which makes it a shared secret — the thing that keeps
internet scanners off the daemon — so it is distributed through an
authenticated share and not a URL. Fetch it through DSM's web UI, which stays
reachable US-wide.

Keep it `0600`. Do not paste it into a terminal transcript, an issue, or a
chat log.

## 2. Bring the tunnel up with the system client

Not because the driver needs it — it does not, and step 4 is the one that
matters — but because this stage fails for reasons that have nothing to do with
any of our code. Doing it first means a failure later has one fewer explanation.

```bash
nix shell nixpkgs#openvpn -c sudo openvpn --config e4e-nas-vpn.ovpn
```

Two checkpoints, in order:

* **`TLS: Initial packet from …`** — this alone proves both that 1194 is open
  from where you are *and* that the HMAC gate accepted the published config,
  because a wrong or absent `ta.key` gets no reply at all rather than an error.
  It does not need credentials: failing afterwards at `AUTH_FAILED` is itself
  evidence that the auth path is enforcing.
* **`Initialization Sequence Completed`** — with real AD credentials.
  Authentication runs OpenVPN → radiusplugin → radiusd → `ntlm_auth` →
  winbind → the DC, entirely inside the NAS, so a DC outage looks like a
  password failure here.

  **The username must carry its domain: `KRG\<ad-user>`, not `<ad-user>`.**
  DSM's `vpnauthd` runs a Synology pre-check — `SYNOWinsEnumAllDomains` and
  the privilege file — ahead of the FreeRADIUS site config. An unqualified
  name matches no local account there, so it is refused as "Incorrect user
  name" before the password is looked at, and never reaches the `Realm` regex
  that would have accepted either form. The driver builds this itself from
  `--domain`; only a hand-run `openvpn` needs it typed.

Do not test with `ping`: ICMP is dropped by the `vpn` firewall adapter by
design. A silent `10.90.24.1` is expected and proves nothing either way.

## 3. Check SMB over that tunnel, without this driver

With the tunnel from step 2 still up, `10.90.24.1:445` is reachable by anything
on the machine. Ask something that is not us:

```bash
smbclient -L //10.90.24.1 -U 'KRG\<ad-user>'
smbclient //10.90.24.1/<share> -U 'KRG\<ad-user>' -c 'ls'
```

What this settles is the half of the question that has nothing to do with our
code: that DSM answers SMB3 on the far side of its own tunnel, to an AD account,
with NTLMv2 and server signing. If this fails, step 4 was never going to work
and the reason is not ours.

> **Not this driver's mount, deliberately.** Pointing
> `synology-filestation-fuse` at a tunnel somebody else raised does not
> currently pick the SMB leg: the chain probes SMB at `--host`, which is the
> public name, and the tunnel routes only `10.90.24.0/24` — so the probe fails
> and the mount falls to HTTP. An externally-raised tunnel is a case the chain
> does not model, and it is worth deciding whether it should before somebody
> discovers it the hard way. Step 4 is the supported shape.

## 4. Mount with no tunnel at all

Stop the `openvpn` from step 2 — including whatever it did to the routing table
— and confirm `10.90.24.1` is unreachable again. Then:

```bash
synology-filestation-fuse \
  --host e4e-nas.ucsd.edu \
  --vpn-host 10.90.24.1 \
  --vpn-profile ~/e4e-nas-vpn.ovpn \
  --vpn-profile-nas /installers/e4e-nas-vpn.ovpn \
  --domain KRG \
  --username <ad-user> \
  ~/mnt
```

No `sudo`, no `tun0`, nothing in `ip route` that was not there before, and
nothing changed for anything else the machine is doing. The tunnel is inside
this process and so is the TCP stack that speaks through it. What proves it
worked is the line:

```
Transport: SMB via VPN at 10.90.24.1
```

A mount that is already running on the HTTP API when the tunnel starts working
does not need a remount: within a minute it logs
`Transport: now SMB via VPN at 10.90.24.1, was HTTPS`. Back on campus it moves
on again, to `Transport: now SMB at <nas>, was SMB via VPN at 10.90.24.1`.

`--vpn-profile` is where the copy lives on *this machine*; `--vpn-profile-nas`
is where it lives on the NAS. If the first does not exist yet it is fetched
from the second, over the session just authenticated — worth testing on
purpose, since it is what somebody off campus with no tunnel has to do to get
the file that gives them one. There is no default for the NAS path: where an
appliance publishes such a file is a local decision, and this client does not
assume one.

Then copy a large file in. This is the first real exercise of three things that
have only ever run against a container: the replacing rename,
`set_end_of_file` truncation, and streamed write-through. Step 3 proved DSM
answers SMB; this is the first time our own SMB code has spoken to it, over a
tunnel our own code is carrying.

## 5. What to watch for

* **Kerberos → NTLMv2 fallback.** Clients cannot reach the DC through this
  tunnel, by design, so an SPN cannot be acquired and SMB falls back to NTLMv2,
  which the NAS pass-through-authenticates over its own network path. That has
  to work against `smb-globals.yml`'s `min_protocol SMB3` and server signing.
  Windows tries Kerberos first wherever it can resolve an SPN, so test a real
  Windows and a real macOS client before announcing the service, not just this
  driver.
* **Isolation.** A traceroute to anything else on campus must not go through
  the tunnel, and the DSM UI on `:6021` must not be reachable from inside it.
  If either is, `allow_lan` has been flipped and the NAS is routing onto the
  campus subnet.
* **Throughput and stalls.** `reneg-sec` defaults to an hour, so any copy that
  runs longer renegotiates mid-transfer. A copy that dies at roughly the
  one-hour mark is that, not the network.

## Reading a failure

Four stages that can fail, and each says so in its own words. The point of
running them in order is that the first one to fail is the one to fix.

| What you see | What it means |
|---|---|
| Step 2 never reaches `TLS: Initial packet` | 1194 is not open from where you are, or `ta.key` does not match. Nothing downstream is worth trying. |
| Step 2 reaches `AUTH_FAILED` | The username was not domain-qualified (`KRG\<ad-user>`), or the credentials, or the DC behind them. The tunnel itself is fine. ⚠️ DSM auto-block is 3 strikes / 24h / never expires, so fix it before the next attempt rather than guessing. |
| Step 3's `smbclient` fails | DSM's SMB, not ours: the dialect, the signing, or the AD account. Step 4 was never going to work. |
| `VPN profile: cannot read … ; the tunnel leg will not be available` | The profile is not where `--vpn-profile` says and could not be fetched. Check `--vpn-profile-nas`, and that the local path is one you can write to. The mount is on HTTP. |
| `the vpn tunnel to … did not come up` | Our client could not do what step 2 did. That is a client bug, and step 2 passing is what makes it one. |
| `the tunnel is up, but nothing answered at 10.90.24.1:445 inside it` | The tunnel carried packets and SMB did not answer through it. Different problem, deliberately worded to be a different sentence. |
| `Transport: no SMB session through the tunnel (…); staying on HTTPS` | The connection was made and the SMB session failed on top of it — dialect, signing. The tunnel is then left alone for ten minutes rather than raised every minute. |
| `SMB: the server refused the login (…)` | The SMB login itself was rejected — usually an AD account with no domain set. It is not retried until the share is reconnected: DSM's auto-block would make a retry loop permanent. |
| `Transport: HTTPS` with nothing above it | No leg was reachable, which is the fallback working. The mount keeps looking, and logs `Transport: now …, was HTTPS` when a leg answers. |

## Why this gated the in-process client

`rust/synology-filestation-openvpn` exists to remove the `sudo openvpn` of step
2 — same tunnel, no tun device, no privileged helper, no effect on the machine's
other traffic. Every line of it was written on the assumption that what is on
the far side is worth reaching, which is why this page came first.

It is now built, and steps 3 and 4 are the difference: the same mount, over a
tunnel the operating system joined and over one this process holds by itself.
Until step 4 has been walked through once against real DSM, treat it as
unvalidated — it has been proved against captured bytes, against OpenSSL,
against a real `openvpn` process, and against a peer in this process that
encrypts and decrypts a whole TCP conversation. None of those is a NAS.
