# Changelog

## [0.7.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.6.0...synology_filestation-v0.7.0) (2026-09-14)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.6.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.5...synology_filestation-v0.6.0) (2026-09-12)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.5](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.4...synology_filestation-v0.5.5) (2026-08-31)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.4](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.3...synology_filestation-v0.5.4) (2026-08-26)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.2...synology_filestation-v0.5.3) (2026-08-26)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.1...synology_filestation-v0.5.2) (2026-08-26)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.5.0...synology_filestation-v0.5.1) (2026-08-26)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.5.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.4.3...synology_filestation-v0.5.0) (2026-08-26)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.4.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.4.2...synology_filestation-v0.4.3) (2026-08-25)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.4.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.4.1...synology_filestation-v0.4.2) (2026-08-25)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.4.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.4.0...synology_filestation-v0.4.1) (2026-08-25)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.4.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.3.1...synology_filestation-v0.4.0) (2026-08-25)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.3.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.3.0...synology_filestation-v0.3.1) (2026-08-12)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.3.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.2.1...synology_filestation-v0.3.0) (2026-08-12)


### ⚠ BREAKING CHANGES

* TLS certificate verification is now on by default. A stock DSM appliance presents a self-signed certificate and will be rejected until it is trusted or verification is explicitly turned off.

### Features

* verify TLS certificates by default, and keep credentials out of the URL ([6594fda](https://github.com/UCSD-E4E/synology-filestation/commit/6594fdab53978d6f2e3de454d05aa338a848b917))


### Bug Fixes

* **core:** carry the session id in a cookie instead of the request URL ([740e8ff](https://github.com/UCSD-E4E/synology-filestation/commit/740e8ff59c4490627912944cebb3e6f868571ee0))
* stop leaking the session id and passwords into logs ([1ad289a](https://github.com/UCSD-E4E/synology-filestation/commit/1ad289a27a7df52121e59b57c72d67c47dd66a3c))

## [0.2.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.2.0...synology_filestation-v0.2.1) (2026-07-30)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.2.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.1.20...synology_filestation-v0.2.0) (2026-07-29)


### Features

* auto-prefer SMB in all consumers (transparent, no API change) ([e9c123a](https://github.com/UCSD-E4E/synology-filestation/commit/e9c123a69c4ec9556dfed02d5420244f02ac5a28))
* streaming reads — fetch large files straight to disk (+ auto_attach) ([6f7d473](https://github.com/UCSD-E4E/synology-filestation/commit/6f7d473085c73fdfe98ea17c29f4418f3e9544ad))
* streaming reads (fetch large files straight to disk) ([7bd32f3](https://github.com/UCSD-E4E/synology-filestation/commit/7bd32f3fce5ed601f9d820b9078190fd82a0eccc))
* streaming writes — stage large files without buffering in memory ([6202d71](https://github.com/UCSD-E4E/synology-filestation/commit/6202d71dc8921742a27f11a6a42ea3579a5815e5))
* streaming writes (stage large files without buffering in memory) ([8cf31a2](https://github.com/UCSD-E4E/synology-filestation/commit/8cf31a2546f583131b7c611bb9ca3595a29d6af8))
* throttle bulk transfers to protect the NAS from saturation ([8ee8d7a](https://github.com/UCSD-E4E/synology-filestation/commit/8ee8d7a3534cb2af2e58ae2a4d407ce1ed4ed1f7))
* throttle bulk transfers to protect the NAS from saturation ([83ddfac](https://github.com/UCSD-E4E/synology-filestation/commit/83ddfaca72952e44193899bf00faa62affdf5dd4))
* transparent SMB-preferred read/write with HTTP fallback (selection + auto-wiring) ([9282f01](https://github.com/UCSD-E4E/synology-filestation/commit/9282f016c0467fbfb63aa19f6c221f0dab262785))


### Bug Fixes

* make internal core dependency path-only so 0.x bumps resolve ([71152cb](https://github.com/UCSD-E4E/synology-filestation/commit/71152cb28ad8dbab1dd85aefe771530acdf00562))
* make internal workspace deps path-only so 0.x bumps resolve ([da68cf8](https://github.com/UCSD-E4E/synology-filestation/commit/da68cf81ab6fea58a2b1dbfe2693e82f2ddf752d))


### Miscellaneous Chores

* release 0.2.0 ([fae7859](https://github.com/UCSD-E4E/synology-filestation/commit/fae7859d5a3489c0b07343768af772f3fb2edcce))

## [0.1.20](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.1.19...synology_filestation-v0.1.20) (2026-06-04)


### Features

* GUI calls the Rust core directly via a native FFI binding ([603e47d](https://github.com/UCSD-E4E/synology-filestation/commit/603e47de958a787974c387d1e98e28688344b30b))


### Bug Fixes

* address code-review findings (correctness, cleanup, altitude) ([7631694](https://github.com/UCSD-E4E/synology-filestation/commit/763169458a7e11041726c29a5941b04bbaa84bce))

## [0.1.19](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.1.18...synology_filestation-v0.1.19) (2026-05-07)


### Miscellaneous Chores

* **synology_filestation:** Synchronize synology-filestation versions

## [0.1.18](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.1.17...synology_filestation-v0.1.18) (2026-05-07)


### Bug Fixes

* **python:** bump requires-python to &gt;=3.10 ([54b35c4](https://github.com/UCSD-E4E/synology-filestation/commit/54b35c4454f806df325f3b0dfcae50972061badf))
* **python:** bump requires-python to &gt;=3.10 to unblock pytest security update ([f5ddba0](https://github.com/UCSD-E4E/synology-filestation/commit/f5ddba0207b14178642b2dc839f0afce7653186a))

## [0.1.17](https://github.com/UCSD-E4E/synology-filestation/compare/synology_filestation-v0.1.16...synology_filestation-v0.1.17) (2026-05-07)


### Features

* Python bindings + fsspec backend (synofs) ([ae100f3](https://github.com/UCSD-E4E/synology-filestation/commit/ae100f372b37f54d3e41aa780db2bfc3f48b0140))
* **python:** PyO3 bindings with sync/async clients and fsspec backend ([a7458fa](https://github.com/UCSD-E4E/synology-filestation/commit/a7458fa5d864749bdff12cbe98f7e5ae4ab6c08b))
