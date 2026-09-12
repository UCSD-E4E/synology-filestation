# Changelog

## [0.6.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.5...synology-filestation-core-v0.6.0) (2026-09-12)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.5.5](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.4...synology-filestation-core-v0.5.5) (2026-08-31)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.5.4](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.3...synology-filestation-core-v0.5.4) (2026-08-26)


### Bug Fixes

* **openvpn:** say why the stack stopped, instead of only that it did ([b11633f](https://github.com/UCSD-E4E/synology-filestation/commit/b11633fe8f26960c796cfe07acbfda04d92a4b6a))
* **openvpn:** say why the stack stopped, instead of only that it did ([614f649](https://github.com/UCSD-E4E/synology-filestation/commit/614f649c5bbd52fcd394791f0086afafca5a1cd1))

## [0.5.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.2...synology-filestation-core-v0.5.3) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.5.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.1...synology-filestation-core-v0.5.2) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.5.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.5.0...synology-filestation-core-v0.5.1) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.5.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.4.3...synology-filestation-core-v0.5.0) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.4.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.4.2...synology-filestation-core-v0.4.3) (2026-08-25)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.4.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.4.1...synology-filestation-core-v0.4.2) (2026-08-25)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.4.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.4.0...synology-filestation-core-v0.4.1) (2026-08-25)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.4.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.3.1...synology-filestation-core-v0.4.0) (2026-08-25)


### Features

* **fuse:** stream writes to the server as they are made ([a438b21](https://github.com/UCSD-E4E/synology-filestation/commit/a438b21ead8ed3a9e98ff37799645ab59a93a57f))
* **fuse:** stream writes to the server as they are made ([aee913a](https://github.com/UCSD-E4E/synology-filestation/commit/aee913a7942c4467cabafef993f490a78c667460))
* **smb:** let SMB take new files, not just replacements ([de61add](https://github.com/UCSD-E4E/synology-filestation/commit/de61add56ae5e2ffe1414319cd22109d4ccfea56))
* **smb:** let SMB take new files, not just replacements ([c5eb0cb](https://github.com/UCSD-E4E/synology-filestation/commit/c5eb0cb4db3245c11b7c8f76438c094ef3e9bfef))
* **smb:** open a file for writing, at an offset ([1a0f8db](https://github.com/UCSD-E4E/synology-filestation/commit/1a0f8db6bb7871bd2b70e5419a7373680dd89594))
* **smb:** open a file for writing, at an offset ([0ae0f25](https://github.com/UCSD-E4E/synology-filestation/commit/0ae0f2572c1b8ab90884812f9df9d42f701ab571))
* **smb:** serve listings and namespace changes over SMB ([422fbf3](https://github.com/UCSD-E4E/synology-filestation/commit/422fbf31ebe825448837e8857b7553d22ea31e13))
* **smb:** serve listings and namespace changes over SMB ([f344684](https://github.com/UCSD-E4E/synology-filestation/commit/f344684dfc597cc1090a1ab9863ace20fe53b6bc))
* **smb:** use the two SET_INFO operations the fork added ([11844c1](https://github.com/UCSD-E4E/synology-filestation/commit/11844c1fbec44ecbe8f86559d451a1cd9af0cdb7))
* **smb:** use the two SET_INFO operations the fork added ([746ed14](https://github.com/UCSD-E4E/synology-filestation/commit/746ed14e0b73f7ee4add655d2cd1c17b4a72f035))


### Bug Fixes

* **core:** a declined backend must not strand its own breaker ([c3dacd1](https://github.com/UCSD-E4E/synology-filestation/commit/c3dacd17fec2fddfedf2ed048ee00a09f7e0643c))
* **core:** address review — the trailing-slash hazard, and real SMB tests ([284130a](https://github.com/UCSD-E4E/synology-filestation/commit/284130afe2695443bf866ca4cb06533118ea59cf))
* **core:** start the file over when DSM disowns the partial ([7b91436](https://github.com/UCSD-E4E/synology-filestation/commit/7b9143662b0b79b0bc39525bf9a722bdff776f16))
* **core:** start the file over when DSM disowns the partial ([5ab59e3](https://github.com/UCSD-E4E/synology-filestation/commit/5ab59e31552260568d5e7de76df0de3d419e70ff))

## [0.3.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.3.0...synology-filestation-core-v0.3.1) (2026-08-12)


### Bug Fixes

* **core:** stop the read timeout from killing large uploads ([da35f24](https://github.com/UCSD-E4E/synology-filestation/commit/da35f24c0475760e1612f6d11b2a2b5ffd4042db))
* **core:** stop the read timeout from killing large uploads ([fd8c4ab](https://github.com/UCSD-E4E/synology-filestation/commit/fd8c4ab6d16d15ee72c2fe4a5c2ddb173270505d))
* **core:** tighten upload verification after review ([2750f15](https://github.com/UCSD-E4E/synology-filestation/commit/2750f15d30668a16ae1df9b60d3041cd93a25a3a))

## [0.3.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.2.1...synology-filestation-core-v0.3.0) (2026-08-12)


### ⚠ BREAKING CHANGES

* TLS certificate verification is now on by default. A stock DSM appliance presents a self-signed certificate and will be rejected until it is trusted or verification is explicitly turned off.

### Features

* **core:** stream large uploads in slices instead of buffering whole files ([0220d73](https://github.com/UCSD-E4E/synology-filestation/commit/0220d735ee1a6d95076e25aaa36a626c4ea0550b))
* **core:** stream large uploads in slices instead of buffering whole files ([e8cce35](https://github.com/UCSD-E4E/synology-filestation/commit/e8cce358a289aab34efe5d71938b6927ae7253ef))
* verify TLS certificates by default, and keep credentials out of the URL ([6594fda](https://github.com/UCSD-E4E/synology-filestation/commit/6594fdab53978d6f2e3de454d05aa338a848b917))


### Bug Fixes

* address review feedback on the slice-upload path ([f525881](https://github.com/UCSD-E4E/synology-filestation/commit/f525881b49e146f5413c655c50f68d39e2ba6e78))
* **core:** carry the session id in a cookie instead of the request URL ([740e8ff](https://github.com/UCSD-E4E/synology-filestation/commit/740e8ff59c4490627912944cebb3e6f868571ee0))
* **core:** fetch whole listings and re-clear before each upload retry ([8d6f3e7](https://github.com/UCSD-E4E/synology-filestation/commit/8d6f3e7ecc4940de16ab91d38c9813e75f7d134c))
* **core:** preserve the underlying cause of a transport error ([0290c91](https://github.com/UCSD-E4E/synology-filestation/commit/0290c91ca18679dcd42f9f9580d26b1ea75011f4))
* **core:** scrub secrets from error messages and logs ([f25012c](https://github.com/UCSD-E4E/synology-filestation/commit/f25012cfa02b09c19e08023635f3cfc5b962cc05))
* seven data-loss and corruption defects found in review (tier 1) ([6b044aa](https://github.com/UCSD-E4E/synology-filestation/commit/6b044aad69c4ed0f4ab84807ec6a1cc5ab8cc7ae))
* stop leaking the session id and passwords into logs ([1ad289a](https://github.com/UCSD-E4E/synology-filestation/commit/1ad289a27a7df52121e59b57c72d67c47dd66a3c))

## [0.2.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.2.0...synology-filestation-core-v0.2.1) (2026-07-30)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.2.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.1.20...synology-filestation-core-v0.2.0) (2026-07-29)


### Features

* streaming reads — fetch large files straight to disk (+ auto_attach) ([6f7d473](https://github.com/UCSD-E4E/synology-filestation/commit/6f7d473085c73fdfe98ea17c29f4418f3e9544ad))
* streaming reads (fetch large files straight to disk) ([7bd32f3](https://github.com/UCSD-E4E/synology-filestation/commit/7bd32f3fce5ed601f9d820b9078190fd82a0eccc))
* streaming writes — stage large files without buffering in memory ([6202d71](https://github.com/UCSD-E4E/synology-filestation/commit/6202d71dc8921742a27f11a6a42ea3579a5815e5))
* streaming writes (stage large files without buffering in memory) ([8cf31a2](https://github.com/UCSD-E4E/synology-filestation/commit/8cf31a2546f583131b7c611bb9ca3595a29d6af8))
* throttle bulk transfers to protect the NAS from saturation ([8ee8d7a](https://github.com/UCSD-E4E/synology-filestation/commit/8ee8d7a3534cb2af2e58ae2a4d407ce1ed4ed1f7))
* throttle bulk transfers to protect the NAS from saturation ([83ddfac](https://github.com/UCSD-E4E/synology-filestation/commit/83ddfaca72952e44193899bf00faa62affdf5dd4))
* transparent read/write backend selection with circuit-breaker fallback ([090d032](https://github.com/UCSD-E4E/synology-filestation/commit/090d0328455b06682d88408abce369f2606d490b))
* transparent SMB-preferred read/write with HTTP fallback (selection + auto-wiring) ([9282f01](https://github.com/UCSD-E4E/synology-filestation/commit/9282f016c0467fbfb63aa19f6c221f0dab262785))


### Bug Fixes

* address Copilot review on the write path ([3db9c36](https://github.com/UCSD-E4E/synology-filestation/commit/3db9c36a36a43b96c43c8a0ad206e1774aa1a981))


### Miscellaneous Chores

* release 0.2.0 ([fae7859](https://github.com/UCSD-E4E/synology-filestation/commit/fae7859d5a3489c0b07343768af772f3fb2edcce))

## [0.1.20](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.1.19...synology-filestation-core-v0.1.20) (2026-06-04)


### Features

* GUI calls the Rust core directly via a native FFI binding ([603e47d](https://github.com/UCSD-E4E/synology-filestation/commit/603e47de958a787974c387d1e98e28688344b30b))


### Bug Fixes

* address code-review findings (correctness, cleanup, altitude) ([7631694](https://github.com/UCSD-E4E/synology-filestation/commit/763169458a7e11041726c29a5941b04bbaa84bce))

## [0.1.19](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.1.18...synology-filestation-core-v0.1.19) (2026-05-07)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.1.18](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.1.17...synology-filestation-core-v0.1.18) (2026-05-07)


### Miscellaneous Chores

* **synology-filestation-core:** Synchronize synology-filestation versions

## [0.1.17](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-core-v0.1.16...synology-filestation-core-v0.1.17) (2026-05-07)


### Features

* **core:** auto-relogin, atomic download_to_path, JSON envelope detection ([fb9d4ac](https://github.com/UCSD-E4E/synology-filestation/commit/fb9d4acf01d920fe7e1d30dd434a662d7693e87a))
* Python bindings + fsspec backend (synofs) ([ae100f3](https://github.com/UCSD-E4E/synology-filestation/commit/ae100f372b37f54d3e41aa780db2bfc3f48b0140))
