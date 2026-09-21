# Third-party notices

Plamenu depends on the Rust crates recorded in `Cargo.lock`. Their applicable
licenses are checked by `cargo-deny`; source and license links are available
from each crate's package metadata.

`crates/server/src/web/assets/altcha.min.js` is the ALTCHA 3.2.3 widget
distribution artifact from the official npm package `altcha@3.2.3`. Its
SHA-256 is
`102bb89eb6ee4556068e2514880b7755495b23d90438c751809cb4f0ecbd4efb`.
The exact upstream MIT license is included as
`LICENSES/ALTCHA-LICENSE.txt`.

`crates/server/src/web/assets/hls.min.js` is the hls.js 1.6.16 distribution
artifact from
<https://cdn.jsdelivr.net/npm/hls.js@1.6.16/dist/hls.min.js>. The unavailable
source-map trailer was removed; the remaining file's SHA-256 is
`2e4247ed8941de61de073b55f6ca72d14b1b1926ffcbcd3759cd9e46f869cd6d`.
The exact upstream license/notice is included as
`LICENSES/hls.js-LICENSE.txt` (SHA-256
`ca8773cf798c7ed997d4dd7c8e23c348699f8d5b7462636694cc14de6cda12db`).

`crates/server/src/web/assets/emoji-17.0.json` is generated from Unicode's
Emoji 17.0 `emoji-test.txt` data. The immutable input is
<https://www.unicode.org/Public/17.0.0/emoji/emoji-test.txt> (SHA-256
`1d8a944f88d7952f7ef7c5167fef3c67995bcae24543949710231b03a201acda`).
`scripts/generate-emoji-catalog.py` keeps fully-qualified emoji and isolated
components, preserves group/subgroup order, and emits compact JSON; the checked
artifact's SHA-256 is
`7e971bbe6ea727c257a42c173395b410f558bdc020c10c5316159e3b7f9fff8a`.
Copyright © 1991-2026 Unicode, Inc. The exact Unicode License V3 notice is
included as `LICENSES/Unicode-3.0.txt` (SHA-256
`e7a93b009565cfce55919a381437ac4db883e9da2126fa28b91d12732bc53d96`).

Test fixtures under `crates/server/tests/fixtures/` are reduced ActivityPub wire
examples used for interoperability testing. Their provenance and transformation
notes, including the exact Lemmy source revision and license, are documented in
that directory.
