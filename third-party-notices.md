# Dependency and asset notes

This record describes dependency and asset notices. The release ZIP includes
`THIRD-PARTY-NOTICES.txt`, generated from the locked Windows runtime dependency
graph with declared licenses, repository URLs and license/notice file contents.
`Cargo.lock` records the exact resolved Rust dependency versions and package
checksums. Resolved dependencies retain their respective MIT, Apache-2.0 or
other applicable licenses.

The application draws its native UI and menu symbols. The application icon
(`assets/icon.png`, `assets/icon.ico`) was created for Switchmut with ChatGPT
image generation and is embedded in `Switchmut.exe`. It is licensed with the
rest of Switchmut under the GNU Affero General Public License, version 3 or
later.

No source code or runtime dependency from a sibling checkout is included.
Switchmut-authored source is licensed under the GNU Affero General Public
License, version 3 or later. Third-party components retain their respective
licenses.
