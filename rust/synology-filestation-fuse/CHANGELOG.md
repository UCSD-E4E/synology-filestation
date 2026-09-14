# Changelog

## [0.7.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.6.0...synology-filestation-fuse-v0.7.0) (2026-09-14)


### Features

* **fuse:** keep the log after the machine goes down ([1412770](https://github.com/UCSD-E4E/synology-filestation/commit/141277065150eef4bcc6a515a8f4fcb882a5c533))
* **fuse:** keep the log after the machine goes down ([64cfabe](https://github.com/UCSD-E4E/synology-filestation/commit/64cfabe9f850d55f3d9e9e0fbaefaaa5442ab171))


### Bug Fixes

* **fuse:** three ways the new log file could still let you down ([a2cd804](https://github.com/UCSD-E4E/synology-filestation/commit/a2cd8043f6105dc87a42e35501e39a782447bfd4))

## [0.6.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.5...synology-filestation-fuse-v0.6.0) (2026-09-12)


### Features

* **fuse:** say why the SMB stream died, not only that it did ([f40184a](https://github.com/UCSD-E4E/synology-filestation/commit/f40184acd473ae94cc65728c469820adfa550081))


### Bug Fixes

* **fuse:** a redial that finds the NAS directly is still a redial ([bf3075b](https://github.com/UCSD-E4E/synology-filestation/commit/bf3075b0e832af9455384c7c5b5b3ce231bedebd))
* **fuse:** a speculative claim must mean a download that is on the wire ([b1154e9](https://github.com/UCSD-E4E/synology-filestation/commit/b1154e960348f50c5aed1eab94d3b9079f4da5f8))
* **fuse:** speculate only where the caller will read what we fetched ([8779914](https://github.com/UCSD-E4E/synology-filestation/commit/877991472177c46882d3ea4a93b86ca199f02c69))
* **fuse:** stop a slow block download from being mistaken for a dead one ([de8c3c6](https://github.com/UCSD-E4E/synology-filestation/commit/de8c3c6acdc4969619a86e35bb432eb4d133c597))
* **smb:** a tunnelled mount must be able to come back ([13bde98](https://github.com/UCSD-E4E/synology-filestation/commit/13bde98e54b36c2b022dd0ba6ed5ad67c25c2510))
* **smb:** clearing the handle cache must take the parked handles too ([9c28686](https://github.com/UCSD-E4E/synology-filestation/commit/9c28686f82803958d8964c39b7ca03ef41276ab9))


### Performance Improvements

* **smb:** stop paying three round trips to move one block ([622c70d](https://github.com/UCSD-E4E/synology-filestation/commit/622c70d9861cf3a75473c98038c1ad6c38444cf4))

## [0.5.5](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.4...synology-filestation-fuse-v0.5.5) (2026-08-31)


### Bug Fixes

* **fuse:** a dead stream must not fail a write nothing has left yet ([5e7d5fb](https://github.com/UCSD-E4E/synology-filestation/commit/5e7d5fb1b15c458e69986dca719ee63180da638a))
* **fuse:** a dead stream must not fail a write nothing has left yet ([b1edb69](https://github.com/UCSD-E4E/synology-filestation/commit/b1edb6992a46d2fd236e80fb8a9b4d7ced127fc3))
* **fuse:** do not spill into the working directory on Windows ([0e0a920](https://github.com/UCSD-E4E/synology-filestation/commit/0e0a92002f152bc702fdc3a7258d8db05fc5cbe7))
* **fuse:** do not spill into the working directory on Windows ([35507b9](https://github.com/UCSD-E4E/synology-filestation/commit/35507b99c70c8f358459ba90711e4bb88b9ee6c5))
* **fuse:** keep spilling after the shell that started the mount is gone ([0b9be75](https://github.com/UCSD-E4E/synology-filestation/commit/0b9be756f89d4c4eb41163ceadb5662582a57050))
* **fuse:** keep spilling after the shell that started the mount is gone ([915bd71](https://github.com/UCSD-E4E/synology-filestation/commit/915bd7181aae49cfba9aeaf668903f600d3b4f4f))
* **fuse:** stop a failed spill from uploading an empty file over the destination ([1eb64a6](https://github.com/UCSD-E4E/synology-filestation/commit/1eb64a668a3e34679b02242bc0b28c653c48bc7b))
* **fuse:** stop a failed spill from uploading an empty file over the destination ([4d505ec](https://github.com/UCSD-E4E/synology-filestation/commit/4d505ecc6ab0e99c0a7044f26e97c48f37b13f1a))

## [0.5.4](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.3...synology-filestation-fuse-v0.5.4) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-fuse:** Synchronize synology-filestation versions

## [0.5.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.2...synology-filestation-fuse-v0.5.3) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-fuse:** Synchronize synology-filestation versions

## [0.5.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.1...synology-filestation-fuse-v0.5.2) (2026-08-26)


### Bug Fixes

* **fuse:** let the kernel cache a directory, and stop mtimes moving ([828d05d](https://github.com/UCSD-E4E/synology-filestation/commit/828d05d43dce362b38d66bb8a16b3a4c20f54841))
* **fuse:** let the kernel cache a directory, and stop mtimes moving ([e9ffe2b](https://github.com/UCSD-E4E/synology-filestation/commit/e9ffe2b1692351bd379f75edea8a34609327fcbb))

## [0.5.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.5.0...synology-filestation-fuse-v0.5.1) (2026-08-26)


### Miscellaneous Chores

* **synology-filestation-fuse:** Synchronize synology-filestation versions

## [0.5.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.4.3...synology-filestation-fuse-v0.5.0) (2026-08-26)


### Bug Fixes

* **fuse:** cache directory listings instead of re-asking the NAS ([a3805ee](https://github.com/UCSD-E4E/synology-filestation/commit/a3805ee5f0a657f612731fce53232ce90ae38666))
* **fuse:** cache directory listings instead of re-asking the NAS ([28cf37f](https://github.com/UCSD-E4E/synology-filestation/commit/28cf37f52b9928962cdfc6eede972aa221123609))

## [0.4.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.4.2...synology-filestation-fuse-v0.4.3) (2026-08-25)


### Bug Fixes

* a disconnect that finishes, and frees the mountpoint ([02956dd](https://github.com/UCSD-E4E/synology-filestation/commit/02956ddf367b6108e5866bc2235d87831fb317f7))
* **fuse:** free the mountpoint without needing sudo ([aca4973](https://github.com/UCSD-E4E/synology-filestation/commit/aca4973642e25cb74bfff27a9a3cedfb96d4637d))

## [0.4.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.4.1...synology-filestation-fuse-v0.4.2) (2026-08-25)


### Bug Fixes

* **vpn:** send the tunnel a domain-qualified login name ([31dc296](https://github.com/UCSD-E4E/synology-filestation/commit/31dc2962420cd042d3e900858a0310984b9bdc0d))
* **vpn:** send the tunnel a domain-qualified login name ([16f232c](https://github.com/UCSD-E4E/synology-filestation/commit/16f232c3eb50ae375f047c17bbd687f481be0a5e))

## [0.4.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.4.0...synology-filestation-fuse-v0.4.1) (2026-08-25)


### Bug Fixes

* stop assuming where a NAS keeps things, and which path was meant ([20899bf](https://github.com/UCSD-E4E/synology-filestation/commit/20899bfa013643f9584dbb5bd01a1c8214b46380))
* stop assuming where a NAS keeps things, and which path was meant ([0ac1397](https://github.com/UCSD-E4E/synology-filestation/commit/0ac1397aff84a1d6ae74fd04749db85bdde58885))

## [0.4.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.3.1...synology-filestation-fuse-v0.4.0) (2026-08-25)


### Features

* **cli:** choose the transport through the chain, and expose the flags ([670f50a](https://github.com/UCSD-E4E/synology-filestation/commit/670f50ae1b5ec6850193eae444eeec434f585e46))
* **cli:** choose the transport through the chain, and expose the flags ([ed55c48](https://github.com/UCSD-E4E/synology-filestation/commit/ed55c48ac6ca62eecebd04e8183219a0a29b8319))
* **connect:** the tunnel leg answers with a connection, not an address ([c70adcc](https://github.com/UCSD-E4E/synology-filestation/commit/c70adcc0e47d0b3518f59905a3f915eaf51171ae))
* **fuse:** mount through a tunnel the mount raises itself ([a12ab11](https://github.com/UCSD-E4E/synology-filestation/commit/a12ab11d7fb6d454c541d71a0a0a6fecb203c662))
* **fuse:** mount through a tunnel the mount raises itself ([ccbf727](https://github.com/UCSD-E4E/synology-filestation/commit/ccbf727715f480e8305867a0e4fd3717e7d1b12c))
* **fuse:** stream writes to the server as they are made ([a438b21](https://github.com/UCSD-E4E/synology-filestation/commit/a438b21ead8ed3a9e98ff37799645ab59a93a57f))
* **fuse:** stream writes to the server as they are made ([aee913a](https://github.com/UCSD-E4E/synology-filestation/commit/aee913a7942c4467cabafef993f490a78c667460))
* **smb:** use the two SET_INFO operations the fork added ([11844c1](https://github.com/UCSD-E4E/synology-filestation/commit/11844c1fbec44ecbe8f86559d451a1cd9af0cdb7))
* **smb:** use the two SET_INFO operations the fork added ([746ed14](https://github.com/UCSD-E4E/synology-filestation/commit/746ed14e0b73f7ee4add655d2cd1c17b4a72f035))


### Bug Fixes

* **cli:** let the old env knobs reach the chain, and an empty domain mean none ([993e2a0](https://github.com/UCSD-E4E/synology-filestation/commit/993e2a0cd3c86d5895ad50719b3e482d0992a4e1))
* **connect:** an address that cannot be dialled must not look like one ([1b8d894](https://github.com/UCSD-E4E/synology-filestation/commit/1b8d894d9f15fcd74ab9a512ce7b702e2f814425))
* **core:** a declined backend must not strand its own breaker ([c3dacd1](https://github.com/UCSD-E4E/synology-filestation/commit/c3dacd17fec2fddfedf2ed048ee00a09f7e0643c))

## [0.3.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.3.0...synology-filestation-fuse-v0.3.1) (2026-08-12)


### Bug Fixes

* **fuse:** present mounted entries under local ownership, not DSM's ([c2f809c](https://github.com/UCSD-E4E/synology-filestation/commit/c2f809c15c05a89797c83b7fd377f1966e159159))
* **fuse:** present mounted entries under local ownership, not DSM's ([2ccefd3](https://github.com/UCSD-E4E/synology-filestation/commit/2ccefd3c87205626b16c2b13e116e5f323c7c91f))
* **fuse:** run file transfers off the FUSE event loop ([cadf83b](https://github.com/UCSD-E4E/synology-filestation/commit/cadf83b05af02fe1bd073944d43c31012464ec05))
* **fuse:** run file transfers off the FUSE event loop ([8a1472b](https://github.com/UCSD-E4E/synology-filestation/commit/8a1472bc4578f42361140adde9648f785b7d93d5))

## [0.3.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.2.1...synology-filestation-fuse-v0.3.0) (2026-08-12)


### ⚠ BREAKING CHANGES

* TLS certificate verification is now on by default. A stock DSM appliance presents a self-signed certificate and will be rejected until it is trusted or verification is explicitly turned off.

### Features

* **core:** stream large uploads in slices instead of buffering whole files ([0220d73](https://github.com/UCSD-E4E/synology-filestation/commit/0220d735ee1a6d95076e25aaa36a626c4ea0550b))
* **core:** stream large uploads in slices instead of buffering whole files ([e8cce35](https://github.com/UCSD-E4E/synology-filestation/commit/e8cce358a289aab34efe5d71938b6927ae7253ef))
* **fuse:** add --password-stdin, and warn when the password comes from argv ([6193e2b](https://github.com/UCSD-E4E/synology-filestation/commit/6193e2b2cf80fad425705e6cfc2e2405ddb2f6d1))
* verify TLS certificates by default, and keep credentials out of the URL ([6594fda](https://github.com/UCSD-E4E/synology-filestation/commit/6594fdab53978d6f2e3de454d05aa338a848b917))


### Bug Fixes

* address review feedback on the slice-upload path ([f525881](https://github.com/UCSD-E4E/synology-filestation/commit/f525881b49e146f5413c655c50f68d39e2ba6e78))
* **fuse:** bound the wait for an in-flight read-cache block ([a0822a3](https://github.com/UCSD-E4E/synology-filestation/commit/a0822a35d4149be64ef14da93b49a2df70bb1fc4))
* **fuse:** create write-spill temp files with owner-only permissions ([1e78f20](https://github.com/UCSD-E4E/synology-filestation/commit/1e78f20c9e5bf19ca4445b368caa3c130c0f9d58))
* **fuse:** stop silently losing data on flush, truncate, move and short reads ([7e8687d](https://github.com/UCSD-E4E/synology-filestation/commit/7e8687dd469124a981fb38202450cb6b721d0fc8))
* seven data-loss and corruption defects found in review (tier 1) ([6b044aa](https://github.com/UCSD-E4E/synology-filestation/commit/6b044aad69c4ed0f4ab84807ec6a1cc5ab8cc7ae))
* stop leaking the session id and passwords into logs ([1ad289a](https://github.com/UCSD-E4E/synology-filestation/commit/1ad289a27a7df52121e59b57c72d67c47dd66a3c))
* **webdav:** use the shared upload payload fork ([7f5cf52](https://github.com/UCSD-E4E/synology-filestation/commit/7f5cf528c7607467d5d43375da8720560715a08f))

## [0.2.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.2.0...synology-filestation-fuse-v0.2.1) (2026-07-30)


### Miscellaneous Chores

* **synology-filestation-fuse:** Synchronize synology-filestation versions

## [0.2.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.1.20...synology-filestation-fuse-v0.2.0) (2026-07-29)


### Features

* auto-prefer SMB in all consumers (transparent, no API change) ([e9c123a](https://github.com/UCSD-E4E/synology-filestation/commit/e9c123a69c4ec9556dfed02d5420244f02ac5a28))
* streaming reads — fetch large files straight to disk (+ auto_attach) ([6f7d473](https://github.com/UCSD-E4E/synology-filestation/commit/6f7d473085c73fdfe98ea17c29f4418f3e9544ad))
* streaming reads (fetch large files straight to disk) ([7bd32f3](https://github.com/UCSD-E4E/synology-filestation/commit/7bd32f3fce5ed601f9d820b9078190fd82a0eccc))
* streaming writes — stage large files without buffering in memory ([6202d71](https://github.com/UCSD-E4E/synology-filestation/commit/6202d71dc8921742a27f11a6a42ea3579a5815e5))
* streaming writes (stage large files without buffering in memory) ([8cf31a2](https://github.com/UCSD-E4E/synology-filestation/commit/8cf31a2546f583131b7c611bb9ca3595a29d6af8))
* transparent SMB-preferred read/write with HTTP fallback (selection + auto-wiring) ([9282f01](https://github.com/UCSD-E4E/synology-filestation/commit/9282f016c0467fbfb63aa19f6c221f0dab262785))


### Bug Fixes

* make internal core dependency path-only so 0.x bumps resolve ([71152cb](https://github.com/UCSD-E4E/synology-filestation/commit/71152cb28ad8dbab1dd85aefe771530acdf00562))
* make internal workspace deps path-only so 0.x bumps resolve ([da68cf8](https://github.com/UCSD-E4E/synology-filestation/commit/da68cf81ab6fea58a2b1dbfe2693e82f2ddf752d))


### Miscellaneous Chores

* release 0.2.0 ([fae7859](https://github.com/UCSD-E4E/synology-filestation/commit/fae7859d5a3489c0b07343768af772f3fb2edcce))

## [0.1.20](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.1.19...synology-filestation-fuse-v0.1.20) (2026-06-04)


### Features

* GUI calls the Rust core directly via a native FFI binding ([603e47d](https://github.com/UCSD-E4E/synology-filestation/commit/603e47de958a787974c387d1e98e28688344b30b))
* GUI calls the Rust core directly via a native FFI binding ([8149a2f](https://github.com/UCSD-E4E/synology-filestation/commit/8149a2fcd0fabf1705298725b3c9e5763225fb77))


### Bug Fixes

* address third round of Copilot review feedback ([e60373f](https://github.com/UCSD-E4E/synology-filestation/commit/e60373fee0a4e6fecc33d12ee2b6231b78b906ef))

## [0.1.19](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.1.18...synology-filestation-fuse-v0.1.19) (2026-05-07)


### Bug Fixes

* **fuser:** address Copilot review feedback on PR [#71](https://github.com/UCSD-E4E/synology-filestation/issues/71) ([72e5813](https://github.com/UCSD-E4E/synology-filestation/commit/72e5813b5efe54fa1b3728a89a2d2a362eea122e))

## [0.1.18](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.1.17...synology-filestation-fuse-v0.1.18) (2026-05-07)


### Miscellaneous Chores

* **synology-filestation-fuse:** Synchronize synology-filestation versions

## [0.1.17](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.1.16...synology-filestation-fuse-v0.1.17) (2026-05-07)


### Features

* Python bindings + fsspec backend (synofs) ([ae100f3](https://github.com/UCSD-E4E/synology-filestation/commit/ae100f372b37f54d3e41aa780db2bfc3f48b0140))
