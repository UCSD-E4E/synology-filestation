# Changelog

## [0.6.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.5...synology-filestation-gui-v0.6.0) (2026-09-12)


### Features

* **fuse:** say why the SMB stream died, not only that it did ([f40184a](https://github.com/UCSD-E4E/synology-filestation/commit/f40184acd473ae94cc65728c469820adfa550081))
* **gui:** let the GUI turn the read-ahead window down ([66fd754](https://github.com/UCSD-E4E/synology-filestation/commit/66fd75470840b20388ae2004548c8952b4e2f952))


### Bug Fixes

* **fuse:** a redial that finds the NAS directly is still a redial ([bf3075b](https://github.com/UCSD-E4E/synology-filestation/commit/bf3075b0e832af9455384c7c5b5b3ce231bedebd))
* **fuse:** a speculative claim must mean a download that is on the wire ([b1154e9](https://github.com/UCSD-E4E/synology-filestation/commit/b1154e960348f50c5aed1eab94d3b9079f4da5f8))
* **fuse:** speculate only where the caller will read what we fetched ([8779914](https://github.com/UCSD-E4E/synology-filestation/commit/877991472177c46882d3ea4a93b86ca199f02c69))
* **fuse:** stop a slow block download from being mistaken for a dead one ([de8c3c6](https://github.com/UCSD-E4E/synology-filestation/commit/de8c3c6acdc4969619a86e35bb432eb4d133c597))
* **smb:** a tunnelled mount must be able to come back ([13bde98](https://github.com/UCSD-E4E/synology-filestation/commit/13bde98e54b36c2b022dd0ba6ed5ad67c25c2510))
* **smb:** clearing the handle cache must take the parked handles too ([9c28686](https://github.com/UCSD-E4E/synology-filestation/commit/9c28686f82803958d8964c39b7ca03ef41276ab9))


### Performance Improvements

* **smb:** stop paying three round trips to move one block ([622c70d](https://github.com/UCSD-E4E/synology-filestation/commit/622c70d9861cf3a75473c98038c1ad6c38444cf4))

## [0.5.5](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.4...synology-filestation-gui-v0.5.5) (2026-08-31)


### Bug Fixes

* **fuse:** keep spilling after the shell that started the mount is gone ([0b9be75](https://github.com/UCSD-E4E/synology-filestation/commit/0b9be756f89d4c4eb41163ceadb5662582a57050))
* **fuse:** keep spilling after the shell that started the mount is gone ([915bd71](https://github.com/UCSD-E4E/synology-filestation/commit/915bd7181aae49cfba9aeaf668903f600d3b4f4f))

## [0.5.4](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.3...synology-filestation-gui-v0.5.4) (2026-08-26)


### Bug Fixes

* **openvpn:** a datagram that will not send is not the end of the tunnel ([5f0c42d](https://github.com/UCSD-E4E/synology-filestation/commit/5f0c42d7753704a959839a9bcb089e106e5ceeab))
* **openvpn:** a datagram that will not send is not the end of the tunnel ([8a97de5](https://github.com/UCSD-E4E/synology-filestation/commit/8a97de5b5f0199f232ec5cc645bd8f395bdecd3f))
* **openvpn:** say why the stack stopped, instead of only that it did ([b11633f](https://github.com/UCSD-E4E/synology-filestation/commit/b11633fe8f26960c796cfe07acbfda04d92a4b6a))
* **openvpn:** say why the stack stopped, instead of only that it did ([614f649](https://github.com/UCSD-E4E/synology-filestation/commit/614f649c5bbd52fcd394791f0086afafca5a1cd1))
* **openvpn:** size a datagram so the wire will take it ([406cbf3](https://github.com/UCSD-E4E/synology-filestation/commit/406cbf322046db7be280d0a53d2bf09d691225b6))
* **openvpn:** size a datagram so the wire will take it ([9d893a8](https://github.com/UCSD-E4E/synology-filestation/commit/9d893a88719cebb78f36f0b4e001a623cabe3b3d))
* **smb:** pick up the shorter write queue ([2f93a95](https://github.com/UCSD-E4E/synology-filestation/commit/2f93a954aac9087c907f797d5bad481e3b1ff560))
* **smb:** pick up the shorter write queue ([7359cad](https://github.com/UCSD-E4E/synology-filestation/commit/7359cadf4533aba3155ccb03e3979871c2ddd5cd))

## [0.5.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.2...synology-filestation-gui-v0.5.3) (2026-08-26)


### Performance Improvements

* **openvpn:** raise the window, and say how a transfer is going ([1b7d8ac](https://github.com/UCSD-E4E/synology-filestation/commit/1b7d8ac6bfae1e37e1cd702672d3d32eca2f83b9))
* **openvpn:** stop the tunnel's window being the speed limit ([7d9de90](https://github.com/UCSD-E4E/synology-filestation/commit/7d9de9099717f17adcdd9b0e80c15f2fb0edf8ed))
* **openvpn:** stop the window being the speed limit ([4d0995c](https://github.com/UCSD-E4E/synology-filestation/commit/4d0995c491193ea973d6d4235092a1952cdc62fa))

## [0.5.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.1...synology-filestation-gui-v0.5.2) (2026-08-26)


### Bug Fixes

* **fuse:** let the kernel cache a directory, and stop mtimes moving ([828d05d](https://github.com/UCSD-E4E/synology-filestation/commit/828d05d43dce362b38d66bb8a16b3a4c20f54841))
* **fuse:** let the kernel cache a directory, and stop mtimes moving ([e9ffe2b](https://github.com/UCSD-E4E/synology-filestation/commit/e9ffe2b1692351bd379f75edea8a34609327fcbb))

## [0.5.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.5.0...synology-filestation-gui-v0.5.1) (2026-08-26)


### Bug Fixes

* **openvpn:** stop reporting silence as a refusal, and cover net30 ([6d91597](https://github.com/UCSD-E4E/synology-filestation/commit/6d9159724a86944fd2bcc2f60f232fcdb077d5d5))
* **openvpn:** stop reporting silence as a refusal, and cover net30 ([f29e9c2](https://github.com/UCSD-E4E/synology-filestation/commit/f29e9c29dc9618d24f0d9e1ebea95b5c248e64cd))

## [0.5.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.4.3...synology-filestation-gui-v0.5.0) (2026-08-26)


### Features

* **openvpn:** say what the tunnel negotiated ([fbfa8e3](https://github.com/UCSD-E4E/synology-filestation/commit/fbfa8e39394ca6cf12540abca3c2ee163d990b27))
* **openvpn:** say what the tunnel negotiated ([3628113](https://github.com/UCSD-E4E/synology-filestation/commit/36281130043dc18d3b9b8e1c5520177004285737))


### Bug Fixes

* **fuse:** cache directory listings instead of re-asking the NAS ([a3805ee](https://github.com/UCSD-E4E/synology-filestation/commit/a3805ee5f0a657f612731fce53232ce90ae38666))
* **fuse:** cache directory listings instead of re-asking the NAS ([28cf37f](https://github.com/UCSD-E4E/synology-filestation/commit/28cf37f52b9928962cdfc6eede972aa221123609))

## [0.4.3](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.4.2...synology-filestation-gui-v0.4.3) (2026-08-25)


### Bug Fixes

* a disconnect that finishes, and frees the mountpoint ([02956dd](https://github.com/UCSD-E4E/synology-filestation/commit/02956ddf367b6108e5866bc2235d87831fb317f7))
* **connect:** say when the tunnel was tried and did not reach SMB ([7e6d200](https://github.com/UCSD-E4E/synology-filestation/commit/7e6d2000923f6b5a448263f92d59b628a33c3df1))
* **connect:** say when the tunnel was tried and did not reach SMB ([da512d0](https://github.com/UCSD-E4E/synology-filestation/commit/da512d0f2530b6051388dddf2b0833f0fe387bd1))
* **fuse:** free the mountpoint without needing sudo ([aca4973](https://github.com/UCSD-E4E/synology-filestation/commit/aca4973642e25cb74bfff27a9a3cedfb96d4637d))
* stop a debug-level session drowning the log pane and the window ([25f7cc4](https://github.com/UCSD-E4E/synology-filestation/commit/25f7cc4b4e4e258ed17a2a7644bd41464164e24d))
* stop a debug-level session drowning the log pane and the window ([a6eb6f5](https://github.com/UCSD-E4E/synology-filestation/commit/a6eb6f56ddd4368fe39afee7de2a45b57f1490b0))

## [0.4.2](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.4.1...synology-filestation-gui-v0.4.2) (2026-08-25)


### Bug Fixes

* **vpn:** send the tunnel a domain-qualified login name ([31dc296](https://github.com/UCSD-E4E/synology-filestation/commit/31dc2962420cd042d3e900858a0310984b9bdc0d))
* **vpn:** send the tunnel a domain-qualified login name ([16f232c](https://github.com/UCSD-E4E/synology-filestation/commit/16f232c3eb50ae375f047c17bbd687f481be0a5e))

## [0.4.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.4.0...synology-filestation-gui-v0.4.1) (2026-08-25)


### Bug Fixes

* **gui:** report a slow disconnect without pretending it finished ([2b6c0b3](https://github.com/UCSD-E4E/synology-filestation/commit/2b6c0b3aacdeef4374b36c078c806ebdb5e02f02))
* **gui:** say where a disconnect has got to, instead of "Working…" ([a3ed0d4](https://github.com/UCSD-E4E/synology-filestation/commit/a3ed0d41db1b1efb6f848a2184a31c5101f1307c))
* **gui:** say where a disconnect has got to, instead of "Working…" ([8f51707](https://github.com/UCSD-E4E/synology-filestation/commit/8f51707d254e07202f0a1ef88a748a4599b5a9de))
* **gui:** stop the settings window drawing over itself ([6ce8500](https://github.com/UCSD-E4E/synology-filestation/commit/6ce85002c04e0e29e912db5ec3989c09c014bbbf))
* **gui:** stop the settings window drawing over itself ([c431c12](https://github.com/UCSD-E4E/synology-filestation/commit/c431c123bb8904beff197e7d60fc175d28d00461))
* stop assuming where a NAS keeps things, and which path was meant ([20899bf](https://github.com/UCSD-E4E/synology-filestation/commit/20899bfa013643f9584dbb5bd01a1c8214b46380))
* stop assuming where a NAS keeps things, and which path was meant ([0ac1397](https://github.com/UCSD-E4E/synology-filestation/commit/0ac1397aff84a1d6ae74fd04749db85bdde58885))

## [0.4.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.3.1...synology-filestation-gui-v0.4.0) (2026-08-25)


### Features

* **cli:** choose the transport through the chain, and expose the flags ([670f50a](https://github.com/UCSD-E4E/synology-filestation/commit/670f50ae1b5ec6850193eae444eeec434f585e46))
* **cli:** choose the transport through the chain, and expose the flags ([ed55c48](https://github.com/UCSD-E4E/synology-filestation/commit/ed55c48ac6ca62eecebd04e8183219a0a29b8319))
* **connect:** decide which way to reach the NAS, and re-decide when it changes ([e0838a6](https://github.com/UCSD-E4E/synology-filestation/commit/e0838a6068aa45cdc23d78d2a1bf4bf01cc44d7a))
* **connect:** decide which way to reach the NAS, and re-decide when it changes ([79f69b7](https://github.com/UCSD-E4E/synology-filestation/commit/79f69b77568458725a7ef3b1bcbc94ac26227780))
* **connect:** fetch the tunnel profile over the session we already have ([f7f843e](https://github.com/UCSD-E4E/synology-filestation/commit/f7f843e85bb8fcd749fef605d4d58d067759edba))
* **connect:** fetch the tunnel profile over the session we already have ([54af643](https://github.com/UCSD-E4E/synology-filestation/commit/54af6430154f850516f32331a8b2eb4c125014b2))
* **connect:** probe each leg at its own address ([fd04ac1](https://github.com/UCSD-E4E/synology-filestation/commit/fd04ac1937eeea7008abbbb5951cf85c7656b59a))
* **connect:** probe each leg at its own address ([4f05bf9](https://github.com/UCSD-E4E/synology-filestation/commit/4f05bf96c812261d15300c4637296d31443709db))
* **connect:** raise the tunnel the decision asked for ([feb8b5a](https://github.com/UCSD-E4E/synology-filestation/commit/feb8b5a1093d6277217c766d0a5b29773df08d46))
* **connect:** raise the tunnel the decision asked for ([6e87f39](https://github.com/UCSD-E4E/synology-filestation/commit/6e87f397bf3d62cff6246df029b300d1a457dcb3))
* **connect:** the tunnel leg answers with a connection, not an address ([c70adcc](https://github.com/UCSD-E4E/synology-filestation/commit/c70adcc0e47d0b3518f59905a3f915eaf51171ae))
* **connect:** the tunnel leg answers with a connection, not an address ([d18d525](https://github.com/UCSD-E4E/synology-filestation/commit/d18d52566de4c08310d28c366ae0a6fa07a7fae5))
* **fuse:** mount through a tunnel the mount raises itself ([a12ab11](https://github.com/UCSD-E4E/synology-filestation/commit/a12ab11d7fb6d454c541d71a0a0a6fecb203c662))
* **fuse:** mount through a tunnel the mount raises itself ([ccbf727](https://github.com/UCSD-E4E/synology-filestation/commit/ccbf727715f480e8305867a0e4fd3717e7d1b12c))
* **fuse:** stream writes to the server as they are made ([a438b21](https://github.com/UCSD-E4E/synology-filestation/commit/a438b21ead8ed3a9e98ff37799645ab59a93a57f))
* **fuse:** stream writes to the server as they are made ([aee913a](https://github.com/UCSD-E4E/synology-filestation/commit/aee913a7942c4467cabafef993f490a78c667460))
* **gui:** call it a volume, and name the app NAS Folder Access ([6dd8f9c](https://github.com/UCSD-E4E/synology-filestation/commit/6dd8f9cc863ad45ff0a658a2c4f6abfe0a32b34f))
* **gui:** call it a volume, and name the app NAS Folder Access ([e4084c3](https://github.com/UCSD-E4E/synology-filestation/commit/e4084c3696cafb161a3791b6dfabea0c38dc7611))
* **gui:** tell the user what to fix when connecting fails ([c1d450e](https://github.com/UCSD-E4E/synology-filestation/commit/c1d450e749ef7d566216449f0f819b3501f23b7f))
* **gui:** tell the user what to fix when connecting fails ([6cc8991](https://github.com/UCSD-E4E/synology-filestation/commit/6cc8991aa17b4ac2b0b53b96f71153a3149cbc5b))
* **openvpn:** a TCP stack for the packets nobody else will take ([2015d5c](https://github.com/UCSD-E4E/synology-filestation/commit/2015d5c30971b28965b18364ac7d13f0eacf68ab))
* **openvpn:** a TCP stack for the packets nobody else will take ([9d81cfd](https://github.com/UCSD-E4E/synology-filestation/commit/9d81cfd045da35c97622cf9b319dcbdbe7a49b21))
* **openvpn:** ask what the server wants us to know ([0decf1e](https://github.com/UCSD-E4E/synology-filestation/commit/0decf1e975518d479e382f8d468f313626850bca))
* **openvpn:** ask what the server wants us to know ([b721c10](https://github.com/UCSD-E4E/synology-filestation/commit/b721c1089bd77829be9b0a1cf0e965203f334933))
* **openvpn:** carry payload, and decrypt a real openvpn's keepalive ([8a6e4db](https://github.com/UCSD-E4E/synology-filestation/commit/8a6e4dbfcc92f91ef50a068e3ac6d5b26abfb85c))
* **openvpn:** carry payload, and decrypt a real openvpn's keepalive ([50d8945](https://github.com/UCSD-E4E/synology-filestation/commit/50d89451c787e46887865c1187c70d26197721ec))
* **openvpn:** exchange key material and derive the data-channel keys ([c3425dd](https://github.com/UCSD-E4E/synology-filestation/commit/c3425dd6043e7592715b8c47d608bcf102fa60fe))
* **openvpn:** exchange key material and derive the data-channel keys ([57ef508](https://github.com/UCSD-E4E/synology-filestation/commit/57ef508cb80f3b1dc6373c57e8f29001dfc3a4ad))
* **openvpn:** give the session a tunnel, and keep it alive ([803ac02](https://github.com/UCSD-E4E/synology-filestation/commit/803ac02d9d92dda1680bb6f1d6c5795ca0283cc2))
* **openvpn:** give the session a tunnel, and keep it alive ([253a449](https://github.com/UCSD-E4E/synology-filestation/commit/253a4496582d8763170f37fcf8a0b9074aa819dc))
* **openvpn:** make the control channel behave like a stream ([a1a4baa](https://github.com/UCSD-E4E/synology-filestation/commit/a1a4baabbe5e46ed922b024073686d6625b399e3))
* **openvpn:** make the control channel behave like a stream ([18bf34c](https://github.com/UCSD-E4E/synology-filestation/commit/18bf34c6cb08de1b4ce0c173de20c34d9138d09c))
* **openvpn:** make the tunnel's TCP stack an ordinary async stream ([ca26223](https://github.com/UCSD-E4E/synology-filestation/commit/ca262231a4ac8f9ddd4f608c5b0b293d663e688d))
* **openvpn:** make the tunnel's TCP stack an ordinary async stream ([7fa3c6c](https://github.com/UCSD-E4E/synology-filestation/commit/7fa3c6cddc6c07c069a5decf7b7467dc7ca9d055))
* **openvpn:** open a TCP connection through the tunnel ([cf159e3](https://github.com/UCSD-E4E/synology-filestation/commit/cf159e33a9eda9b9a1ed1239bcaf533e280bf629))
* **openvpn:** open a TCP connection through the tunnel ([19651b7](https://github.com/UCSD-E4E/synology-filestation/commit/19651b7896f1178dd7262832c58097e1faf498eb))
* **openvpn:** read the file users are actually handed ([3495f08](https://github.com/UCSD-E4E/synology-filestation/commit/3495f08e39a866371ad8d4981274425ffce80622))
* **openvpn:** read the file users are actually handed ([7d2506b](https://github.com/UCSD-E4E/synology-filestation/commit/7d2506b1ec8eb974c4abe6b78323fa817b63e995))
* **openvpn:** renegotiate, without stopping the tunnel ([904f9e1](https://github.com/UCSD-E4E/synology-filestation/commit/904f9e1d66630af59389b4f0066c5fbefc8232a9))
* **openvpn:** renegotiate, without stopping the tunnel ([447f11d](https://github.com/UCSD-E4E/synology-filestation/commit/447f11dd5f2bab8fc172702aad72c54b84cf73f4))
* **openvpn:** run a TLS session on the control channel ([8e9e81c](https://github.com/UCSD-E4E/synology-filestation/commit/8e9e81cc78e2ccc33c5b9cddb906bcc17aaaf6b6))
* **openvpn:** run a TLS session on the control channel ([fccb766](https://github.com/UCSD-E4E/synology-filestation/commit/fccb766fb8f948a4e1aa527b6dd8a1424d0e2ed7))
* **openvpn:** speak the tls-auth control channel ([fe6f52f](https://github.com/UCSD-E4E/synology-filestation/commit/fe6f52f02dc09471d73aac80113cec313d407be8))
* **openvpn:** speak the tls-auth control channel ([5ad7a94](https://github.com/UCSD-E4E/synology-filestation/commit/5ad7a9486b7c1e600acaa387bbde92522b58924f))
* **openvpn:** the part that actually runs ([b4f092c](https://github.com/UCSD-E4E/synology-filestation/commit/b4f092ce0c9d42391b1d4f2400e6a80274abfa48))
* **openvpn:** the part that actually runs ([2e2bec9](https://github.com/UCSD-E4E/synology-filestation/commit/2e2bec9fe37e15454b7ccbf81fbad173f602084e))
* **openvpn:** tie the control channel together, and prove it against openvpn ([80a1a75](https://github.com/UCSD-E4E/synology-filestation/commit/80a1a7571f730582254dddfa268d21fd48325007))
* **openvpn:** tie the control channel together, and prove it against openvpn ([3a5eab8](https://github.com/UCSD-E4E/synology-filestation/commit/3a5eab8015cbdf048c88f5197a08de714d002189))
* **smb:** an smb2 transport over any byte stream ([79a58e0](https://github.com/UCSD-E4E/synology-filestation/commit/79a58e041d143dc5aa39305a6bfe4e945b1c1b4a))
* **smb:** an smb2 transport over any byte stream ([08ed3c2](https://github.com/UCSD-E4E/synology-filestation/commit/08ed3c2db2c8b51f9b964875a98e111cfaa52680))
* **smb:** let SMB take new files, not just replacements ([de61add](https://github.com/UCSD-E4E/synology-filestation/commit/de61add56ae5e2ffe1414319cd22109d4ccfea56))
* **smb:** let SMB take new files, not just replacements ([c5eb0cb](https://github.com/UCSD-E4E/synology-filestation/commit/c5eb0cb4db3245c11b7c8f76438c094ef3e9bfef))
* **smb:** open a file for writing, at an offset ([1a0f8db](https://github.com/UCSD-E4E/synology-filestation/commit/1a0f8db6bb7871bd2b70e5419a7373680dd89594))
* **smb:** open a file for writing, at an offset ([0ae0f25](https://github.com/UCSD-E4E/synology-filestation/commit/0ae0f2572c1b8ab90884812f9df9d42f701ab571))
* **smb:** serve listings and namespace changes over SMB ([422fbf3](https://github.com/UCSD-E4E/synology-filestation/commit/422fbf31ebe825448837e8857b7553d22ea31e13))
* **smb:** serve listings and namespace changes over SMB ([f344684](https://github.com/UCSD-E4E/synology-filestation/commit/f344684dfc597cc1090a1ab9863ace20fe53b6bc))
* **smb:** SMB on a stream nobody in this workspace dialled ([81234bc](https://github.com/UCSD-E4E/synology-filestation/commit/81234bc14633f5973a6f8dc305393839c05addd5))
* **smb:** SMB on a stream nobody in this workspace dialled ([c21df54](https://github.com/UCSD-E4E/synology-filestation/commit/c21df54371eb62dfe9834ef9c97c2750a752facb))
* **smb:** use the two SET_INFO operations the fork added ([11844c1](https://github.com/UCSD-E4E/synology-filestation/commit/11844c1fbec44ecbe8f86559d451a1cd9af0cdb7))
* **smb:** use the two SET_INFO operations the fork added ([746ed14](https://github.com/UCSD-E4E/synology-filestation/commit/746ed14e0b73f7ee4add655d2cd1c17b4a72f035))


### Bug Fixes

* **cli:** let the old env knobs reach the chain, and an empty domain mean none ([993e2a0](https://github.com/UCSD-E4E/synology-filestation/commit/993e2a0cd3c86d5895ad50719b3e482d0992a4e1))
* **connect:** an address that cannot be dialled must not look like one ([1b8d894](https://github.com/UCSD-E4E/synology-filestation/commit/1b8d894d9f15fcd74ab9a512ce7b702e2f814425))
* **connect:** give up in a way that lets go ([a35fefd](https://github.com/UCSD-E4E/synology-filestation/commit/a35fefd5af211263cda6d0eddcdf1850bba48eae))
* **connect:** only trust a regular file, and only at a path we can create ([1f92ee0](https://github.com/UCSD-E4E/synology-filestation/commit/1f92ee0948f9c0b95597bc75b216134705607838))
* **core:** a declined backend must not strand its own breaker ([c3dacd1](https://github.com/UCSD-E4E/synology-filestation/commit/c3dacd17fec2fddfedf2ed048ee00a09f7e0643c))
* **core:** address review — the trailing-slash hazard, and real SMB tests ([284130a](https://github.com/UCSD-E4E/synology-filestation/commit/284130afe2695443bf866ca4cb06533118ea59cf))
* **core:** start the file over when DSM disowns the partial ([7b91436](https://github.com/UCSD-E4E/synology-filestation/commit/7b9143662b0b79b0bc39525bf9a722bdff776f16))
* **core:** start the file over when DSM disowns the partial ([5ab59e3](https://github.com/UCSD-E4E/synology-filestation/commit/5ab59e31552260568d5e7de76df0de3d419e70ff))
* **gui:** chain the mount failure's cause, and correct a stale doc ([841ddc9](https://github.com/UCSD-E4E/synology-filestation/commit/841ddc97c70a8bef6c2ead49b2597a17ee77ea7b))
* **gui:** render natively on Wayland, and declare DPI awareness on Windows ([f0d44bb](https://github.com/UCSD-E4E/synology-filestation/commit/f0d44bb92b7b1ad8568ac362430bb92cffd719bc))
* **gui:** render natively on Wayland, and declare DPI awareness on Windows ([371ddc7](https://github.com/UCSD-E4E/synology-filestation/commit/371ddc745e5e8dac9758b2fbf8e1f51455d3274a))
* make the flake read the pin, and stop losing commits to no package ([6acb4a3](https://github.com/UCSD-E4E/synology-filestation/commit/6acb4a3144b825a485407951cbe63d5050a22e95))
* **nix:** ship a desktop entry and icon with the GUI package ([79e4071](https://github.com/UCSD-E4E/synology-filestation/commit/79e4071391c92ef1d02250b8ef5d0b5b13ab7891))
* **nix:** ship a desktop entry and icon with the GUI package ([c290318](https://github.com/UCSD-E4E/synology-filestation/commit/c29031894cdc29fe8d75d4f1657ab2c451e172eb))
* **openvpn:** ask again, refuse compression, and name a key rotation ([dd1c726](https://github.com/UCSD-E4E/synology-filestation/commit/dd1c72621e39efec8ba8ab28ac1b2d15be303d80))
* **openvpn:** blame the tunnel when it is the tunnel ([00b18f2](https://github.com/UCSD-E4E/synology-filestation/commit/00b18f2add64370994b8565ce2221d587ff3fa15))
* **openvpn:** clear the buffers that only looked cleared ([67fa81a](https://github.com/UCSD-E4E/synology-filestation/commit/67fa81a339451bbb390ca26e68cb3db1fa2caa81))
* **openvpn:** close the cross-session latch, and stop spinning ([f447bda](https://github.com/UCSD-E4E/synology-filestation/commit/f447bda06a52680b7a85f92a4b1569a25ef644bf))
* **openvpn:** correct the fatal/not-fatal split, and reorder both ways ([ad514d1](https://github.com/UCSD-E4E/synology-filestation/commit/ad514d1715a6361463c4b01537e23be789d61350))
* **openvpn:** decode hex pairs the way clippy 1.98 asks ([fbcf11c](https://github.com/UCSD-E4E/synology-filestation/commit/fbcf11cbac419cb54f243884f7f5391f78664639))
* **openvpn:** do not let a rejected packet decide who the peer is ([ab5284f](https://github.com/UCSD-E4E/synology-filestation/commit/ab5284fe61e7f5d42026a475c6b0e2a460e5e5a2))
* **openvpn:** enforce the replay header, and stop lying about idleness ([9446a1b](https://github.com/UCSD-E4E/synology-filestation/commit/9446a1bfcd35d719a47786abb126284b7a44adc2))
* **openvpn:** keep a blip a blip, and read the files people are handed ([ff2c01f](https://github.com/UCSD-E4E/synology-filestation/commit/ff2c01fda67dbaae67a3fe08af5e512b46cd945b))
* **openvpn:** keep key material out of memory it does not need to be in ([f851475](https://github.com/UCSD-E4E/synology-filestation/commit/f85147542e99d1560a9f8b25a1e111e692d26483))
* **openvpn:** let fast retransmit actually wake the caller ([d252845](https://github.com/UCSD-E4E/synology-filestation/commit/d252845e30e25ef5fe3010d9ca8a521f63f619df))
* **openvpn:** make a rotation survive the things around it ([39bf502](https://github.com/UCSD-E4E/synology-filestation/commit/39bf502560980767914a6ce376e5976025d70142))
* **openvpn:** make the first-packet rule say what it meant ([43fdfc4](https://github.com/UCSD-E4E/synology-filestation/commit/43fdfc4e396205aa506f1345c4e9e0d19df5107d))
* **openvpn:** make the keepalive one a caller can actually rely on ([a5609ae](https://github.com/UCSD-E4E/synology-filestation/commit/a5609ae58c8b7cf9082f874cd3202c6d6cdc5869))
* **openvpn:** make the keepalive one a caller can actually rely on ([a0d5f72](https://github.com/UCSD-E4E/synology-filestation/commit/a0d5f7299ba61ef80c15fb3a4657b2da7738564d))
* **openvpn:** make the narrow fields narrow in the type system ([c3adbb5](https://github.com/UCSD-E4E/synology-filestation/commit/c3adbb5e22414b6b5c26c2e6d3fff08980596c0b))
* **openvpn:** notice a peer that has gone, and stop parking the loop ([f0d92ba](https://github.com/UCSD-E4E/synology-filestation/commit/f0d92ba44ad53c799ff36191ae013a51c0ac6b02))
* **openvpn:** read the push reply, rather than asking and not listening ([c17285b](https://github.com/UCSD-E4E/synology-filestation/commit/c17285bd5c0f4ae2638b65f70add3619e2438d18))
* **openvpn:** refuse another key's packets instead of misreading them ([1788e5b](https://github.com/UCSD-E4E/synology-filestation/commit/1788e5b3b88d6ea4ed5fa21f52da7c51f49ecd7d))
* **openvpn:** refuse another key's packets instead of misreading them ([23931aa](https://github.com/UCSD-E4E/synology-filestation/commit/23931aa734a037411ff58b23dfd859ba788d677a))
* **openvpn:** remove the window where a session could start wrongly ([67496c5](https://github.com/UCSD-E4E/synology-filestation/commit/67496c5f1fe43a86e4735e1b76bc697d01290c9b))
* **openvpn:** report a failed key-method send instead of explaining it away ([c4d13ee](https://github.com/UCSD-E4E/synology-filestation/commit/c4d13ee2c9d2d48cebff58691476645ba0c69b02))
* **openvpn:** require the peer's reset to be its first message too ([3ac40b0](https://github.com/UCSD-E4E/synology-filestation/commit/3ac40b0a1893efe5c98da30de5e18bc4cce00851))
* **openvpn:** say "wrong password" when the password is wrong ([8b975de](https://github.com/UCSD-E4E/synology-filestation/commit/8b975de6c622641445a73de6cefd5c5002366353))
* **openvpn:** say how the stream ended, rather than that it ended ([9fbc8ea](https://github.com/UCSD-E4E/synology-filestation/commit/9fbc8eacfe9e030867b83491f02e4564961b0342))
* **openvpn:** stop killing a tunnel for being quiet ([3fafc01](https://github.com/UCSD-E4E/synology-filestation/commit/3fafc011f27efded76c1b199a423ecc8c0da1438))
* **openvpn:** stop the keepalive breaking the tests it was added for ([19f81de](https://github.com/UCSD-E4E/synology-filestation/commit/19f81dee9d35c9c66d0d85d4b93d8854441bb052))
* **openvpn:** stop the send window running ahead of the peer ([2283e38](https://github.com/UCSD-E4E/synology-filestation/commit/2283e38f93ff0b00d40c2893f901a93ed7a31aa1))
* **smb:** accept the stream this was written for ([bb7f980](https://github.com/UCSD-E4E/synology-filestation/commit/bb7f98063e72b6e35b2cf8a58306fa49dcb1f274))
* **smb:** three bytes stop one short of 16 MiB, not at it ([9a2ec60](https://github.com/UCSD-E4E/synology-filestation/commit/9a2ec603a7c31d05cf4fb43fb6c84428b8b10588))

## [0.3.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.3.0...synology-filestation-gui-v0.3.1) (2026-08-12)


### Bug Fixes

* **core:** stop the read timeout from killing large uploads ([da35f24](https://github.com/UCSD-E4E/synology-filestation/commit/da35f24c0475760e1612f6d11b2a2b5ffd4042db))
* **core:** stop the read timeout from killing large uploads ([fd8c4ab](https://github.com/UCSD-E4E/synology-filestation/commit/fd8c4ab6d16d15ee72c2fe4a5c2ddb173270505d))
* **core:** tighten upload verification after review ([2750f15](https://github.com/UCSD-E4E/synology-filestation/commit/2750f15d30668a16ae1df9b60d3041cd93a25a3a))
* **fuse:** present mounted entries under local ownership, not DSM's ([c2f809c](https://github.com/UCSD-E4E/synology-filestation/commit/c2f809c15c05a89797c83b7fd377f1966e159159))
* **fuse:** present mounted entries under local ownership, not DSM's ([2ccefd3](https://github.com/UCSD-E4E/synology-filestation/commit/2ccefd3c87205626b16c2b13e116e5f323c7c91f))
* **fuse:** run file transfers off the FUSE event loop ([cadf83b](https://github.com/UCSD-E4E/synology-filestation/commit/cadf83b05af02fe1bd073944d43c31012464ec05))
* **fuse:** run file transfers off the FUSE event loop ([8a1472b](https://github.com/UCSD-E4E/synology-filestation/commit/8a1472bc4578f42361140adde9648f785b7d93d5))

## [0.3.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.2.1...synology-filestation-gui-v0.3.0) (2026-08-12)


### ⚠ BREAKING CHANGES

* TLS certificate verification is now on by default. A stock DSM appliance presents a self-signed certificate and will be rejected until it is trusted or verification is explicitly turned off.

### Features

* **core:** stream large uploads in slices instead of buffering whole files ([e8cce35](https://github.com/UCSD-E4E/synology-filestation/commit/e8cce358a289aab34efe5d71938b6927ae7253ef))
* **fuse:** add --password-stdin, and warn when the password comes from argv ([6193e2b](https://github.com/UCSD-E4E/synology-filestation/commit/6193e2b2cf80fad425705e6cfc2e2405ddb2f6d1))
* verify TLS certificates by default, and keep credentials out of the URL ([6594fda](https://github.com/UCSD-E4E/synology-filestation/commit/6594fdab53978d6f2e3de454d05aa338a848b917))


### Bug Fixes

* stop leaking the session id and passwords into logs ([1ad289a](https://github.com/UCSD-E4E/synology-filestation/commit/1ad289a27a7df52121e59b57c72d67c47dd66a3c))

## [0.2.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-gui-v0.2.0...synology-filestation-gui-v0.2.1) (2026-07-30)


### Features

* **gui:** copy NAS paths from the browser path bar and context menu ([dd63bd6](https://github.com/UCSD-E4E/synology-filestation/commit/dd63bd677af11bb1b8b7ba392a20f6fa80410c15))
* **gui:** copy NAS paths from the browser path bar and context menu ([f718536](https://github.com/UCSD-E4E/synology-filestation/commit/f7185360fdc037b292b99f5cc3c2459534fbbe7b))
