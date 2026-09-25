# Changelog

## [0.8.1](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.8.0...synology-filestation-fuse-v0.8.1) (2026-09-25)


### Bug Fixes

* **fuse:** name the host when the leg changes; test: stop two log tests flaking ([ece4646](https://github.com/UCSD-E4E/synology-filestation/commit/ece4646a7581f8414ad9d966f92a25b0375de65a))

## [0.8.0](https://github.com/UCSD-E4E/synology-filestation/compare/synology-filestation-fuse-v0.7.0...synology-filestation-fuse-v0.8.0) (2026-09-25)


### Features

* **connect:** look for a better leg, and ration the tunnel ([1eb97dc](https://github.com/UCSD-E4E/synology-filestation/commit/1eb97dc31b917f95244e8d8479f0675b7142b104))
* **fuse:** a mount moves to a better leg while it runs ([c1f47b6](https://github.com/UCSD-E4E/synology-filestation/commit/c1f47b6da368997f6e4ff333d3dad341f7515657)), closes [#233](https://github.com/UCSD-E4E/synology-filestation/issues/233)
* **gui:** keep the transport badge current ([a9eb1c6](https://github.com/UCSD-E4E/synology-filestation/commit/a9eb1c6939bc8606412fb8cc7b25c3b0791f484d))
* **smb:** a transport that can be handed a session later ([74ee7eb](https://github.com/UCSD-E4E/synology-filestation/commit/74ee7eb6b9acc6090a70dcff6b1819559a458e07))


### Bug Fixes

* **connect:** ask the tunnel for SMB's configured port ([3e83c81](https://github.com/UCSD-E4E/synology-filestation/commit/3e83c815b38bec686eeb73b84f5342625f1b60fd))
* **core:** a backend that declines a transfer hands it to HTTP ([acbceb5](https://github.com/UCSD-E4E/synology-filestation/commit/acbceb569c774c6e218510e5a17360bc9346cdee))
* **fuse:** bound the direct dial, and ration a tunnel that carries nothing ([31e5be2](https://github.com/UCSD-E4E/synology-filestation/commit/31e5be23b6dd69329a9fa7f3740bacd3d4f3f703))
* stop the leg watch before teardown, and dial SMB's configured port ([06b00cb](https://github.com/UCSD-E4E/synology-filestation/commit/06b00cbea5c424800257d135e7f61699a3ff7a7b))

## 0.1.17 – 0.7.0

From 0.1.17 through 0.7.0 each component was released separately, with its own changelog and its own GitHub release:

* [GUI and installers](SynologyFuse.Gui/CHANGELOG.md)
* [FUSE driver and CLI](rust/synology-filestation-fuse/CHANGELOG.md)
* [Core HTTP client](rust/synology-filestation-core/CHANGELOG.md)
* [Python bindings](python/synology_filestation/CHANGELOG.md)

From 0.8.0 on, everything ships as a single release and is recorded here.

## [0.1.16](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.15...synology-filestation-fuse-v0.1.16) (2026-04-27)


### Features

* show version number in status bar ([4632261](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/4632261f53ea41dfea5d0836e28c83a27df1e3aa))

## [0.1.15](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.14...synology-filestation-fuse-v0.1.15) (2026-04-27)


### Bug Fixes

* fuse driver now recovers if we go offline temporarily ([192575a](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/192575abc69fc9f9454bf02a19067d328cd9c52d))

## [0.1.14](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.13...synology-filestation-fuse-v0.1.14) (2026-04-26)


### Features

* auto remind about version updates ([c5043c9](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/c5043c91f261289f5a859e722f9e5a7ca5229db8))
* ui cleanup.  can now use enter to automatically submit password and otp. ([0d49424](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/0d494245057e3e6602ae89a8497812154a02f72b))

## [0.1.13](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.12...synology-filestation-fuse-v0.1.13) (2026-04-12)


### Bug Fixes

* macos installer on modern mac ([d12d03d](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/d12d03d177340adf39d46bab2731d535e75f9db5))

## [0.1.12](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.11...synology-filestation-fuse-v0.1.12) (2026-04-07)


### Bug Fixes

* **macos:** remove rootVolumeOnly to resolve system volume install error ([5f0b589](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/5f0b5898c8144bdc2d6134ca47c9a528d20126f7))
* remove rootVolumeOnly from macOS installer distribution.xml ([26e36f3](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/26e36f3b66fd3df7172dbbc3b9c58571ad2fbfb8))

## [0.1.11](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.10...synology-filestation-fuse-v0.1.11) (2026-04-05)


### Bug Fixes

* don't release the msi ([bd7f0cb](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/bd7f0cb57794592b05d73cf00231cc0508e910f5))

## [0.1.10](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.9...synology-filestation-fuse-v0.1.10) (2026-04-05)


### Bug Fixes

* try again ([a97816a](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/a97816a8a81a840b47c573d11b9ab663baeeff28))

## [0.1.9](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.8...synology-filestation-fuse-v0.1.9) (2026-04-05)


### Bug Fixes

* windows again again ([3003ea8](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/3003ea8b9e133155e2051b9f6064e8da7bf66d9c))

## [0.1.8](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.7...synology-filestation-fuse-v0.1.8) (2026-04-05)


### Bug Fixes

* windows again ([82b5088](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/82b5088ab7d7621f5a4c8e5e1272d22718272474))

## [0.1.7](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.6...synology-filestation-fuse-v0.1.7) (2026-04-05)


### Bug Fixes

* additional windows fixes.  use older wix ([36dea6e](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/36dea6e4cc9b891db61d1f8c373de03d4ee5e97c))

## [0.1.6](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.5...synology-filestation-fuse-v0.1.6) (2026-04-04)


### Bug Fixes

* make sure everythign releases to the expected location ([2e3d8ad](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/2e3d8ad2ef337f71f1bfb769012b12f3be53d3e1))
* more windows build fixes for release ([51ab9cd](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/51ab9cdbc25ea3bd7e097779672d0549308560c3))

## [0.1.5](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.4...synology-filestation-fuse-v0.1.5) (2026-04-04)


### Bug Fixes

* windows release msi fixes ([98a38e5](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/98a38e5ae9894d495acbc1b7f4e47078966df2c5))
* windows release should output to an expected directory ([c699fbd](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/c699fbd11b74578abfc40220011a6c1b3b5065da))

## [0.1.4](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.3...synology-filestation-fuse-v0.1.4) (2026-04-04)


### Bug Fixes

* windows release msi build ([64eb069](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/64eb069b1dded61dc186dc7197e0caf3766a15f0))

## [0.1.3](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.2...synology-filestation-fuse-v0.1.3) (2026-04-04)


### Features

* build on windows as well ([e96b1f5](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/e96b1f59ff8c74cbd1c080da3e1a67eb19523e11))
* cleanup ([3ee31a4](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/3ee31a4c72a1f322a0221952a3137f42e80a5857))
* dotnet UI ([7af6f0b](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/7af6f0ba2ccd46719b141feb6d3523168d5d4416))
* introduce deb installer ([1002943](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/100294375902bca5d52fca74b336e13e249f4d9c))
* introduce dotnet builds ([544ee76](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/544ee76cb654f79054179b3a15eee996d35043f4))
* introduce unit tests ([b11f43d](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/b11f43d87df03b6363bfdb85236bfeca08f704c8))
* introduce windows installer ([967f27a](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/967f27a860911330bb108e60d43a4b2c3c5c954c))
* macfuse ([370f8ea](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/370f8ea76f1762063d07ce05d75c319bc1b715f4))
* macos installer ([2328442](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/2328442150421f91a38f72b9b02de4d0baedcb14))
* macos installer ([0f83628](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/0f83628168f81c73086499bee974e767f244bd89))
* notify user when NAS is mounted and ready for use ([5299a31](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/5299a3113db9f4f416009bf2eccd8f31f241cdd7))
* performance improvements ([9fb38f4](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/9fb38f44f0167e628e2821d9c231ca60874754a5))
* windows support ([cb6d8fd](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/cb6d8fd9ed940acdb65b71ebc68a128acf80a423))
* windows support ([dc7f552](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/dc7f5522a34a27c2e997f4a774ee88823255fce0))


### Bug Fixes

* add pinned SHA-256 verification for WinFsp MSI download in Build-Bundle.ps1 ([9a4e095](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/9a4e0951314651f9befc85eb091ee8dc13a0b3f9))
* build errors ([bf632d6](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/bf632d6b721f5d1976f8db692a4edf148a7510d2))
* build issues for macos and windows on github actions ([e6c9a32](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/e6c9a32df6db2b774b5d4817c5f11261ee827ff4))
* can now launch executable after install.  better error handling.  folder path is now created ([4dd46b0](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/4dd46b013f1fab75ee37058190a6d10e499bbbfc))
* make sure we include the assets folder ([bbdefbe](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/bbdefbeb7d146c086094024f81ac9c5752a03d6c))
* remove bad comment ([c86fb29](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/c86fb296ed1f50893e913c72d784e2bdaf7e93a1))
* rust buildtime checks for windows ([199e5ce](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/199e5ce44f9b45c87dcf5b094cae1e4e11a8a2c5))
* timeout errors ([b0b7b12](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/b0b7b12e21d0abdce96a43bb0ac236ac31c3300e))
* windows build error ([7b5cf70](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/7b5cf70f3057e4365041202b86a516bebb843b4b))
* working installer ([fcb7f07](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/fcb7f072fdebc2bafd3dcedc0f01b66991e9c188))

## [0.1.2](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.1...synology-filestation-fuse-v0.1.2) (2026-03-27)


### Features

* macos support ([abd6207](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/abd6207616fe0b5ff8a0ba148c2078dcaec2ad41))


### Bug Fixes

* cache issues ([5235928](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/52359285201b3f718483b1045f73f96a66cc511c))

## [0.1.1](https://github.com/UCSD-E4E/synology-filestation-fuse/compare/synology-filestation-fuse-v0.1.0...synology-filestation-fuse-v0.1.1) (2026-03-25)


### Features

* allow password via stdin ([32f723c](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/32f723c308e4675249377954be42d6c01342f518))
* cache ([ff29881](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/ff298818aab98b8990ceec36c06ffce3a9012265))
* initial commit ([2b68bee](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/2b68bee40082a8e265ffe3549bca4ad40f03064a))
* introduce github actions ([7f36c03](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/7f36c0358f460cf525b24f114553de478d9f349e))
* release management ([60fd9b0](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/60fd9b07566d11cb8506a54d582d2bff95d35b41))
* unit tests ([26ee7cc](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/26ee7ccf7791fa2b4bb5405494a9cea936bbf2b4))


### Bug Fixes

* can now write file with vim ([3969eb0](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/3969eb03b4f76134c5b3034ef9d7d824662585c6))
* don't incorrectly filter the visible shares ([5dc0316](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/5dc031698b7d494a80224c7a8ebb37339456f869))
* install libfuse3-dev ([785d05d](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/785d05d42b8e371331807419d09ce268056939b9))
* touching a file works ([e58576b](https://github.com/UCSD-E4E/synology-filestation-fuse/commit/e58576b7383c18a76cf6964c953ef53a9ffbae44))
