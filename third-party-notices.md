# Dependency and asset notes

This repository-only record describes dependency and asset notices. It is
not copied into the portable package; the release ZIP contains only
`Switchmut.exe`. `Cargo.lock` records the exact resolved Rust dependency
versions and package checksums. Resolved dependencies retain their respective
MIT, Apache-2.0 or other applicable licenses.

The behavior and executable identity constants were checked against the local
FFXIV Switch Monitor v2.7 C# source. Its supplied tree did not contain a license
file or populated assembly copyright. Legacy binaries and audio files are not
bundled. The application draws its native UI and menu symbols. The gamepad icon
is a separate repository asset and is not covered by the Switchmut source
license; this file makes no additional provenance or redistribution claim for
it.

No source code or runtime dependency from a sibling checkout is included.
Switchmut-authored source is licensed under the GNU Affero General Public
License, version 3 or later. Third-party components retain their respective
licenses.
