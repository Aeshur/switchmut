<h1 align="center">Switchmut</h1>

<p align="center">
<img src="assets/icon.png" alt="Switchmut logo" width="128">
</p>

<p align="center">
Compact tool for switching Final Fantasy XIV 1.23b clients with a
Gamepad.
</p>

<p align="center">
<a href="LICENSE"><img src="https://img.shields.io/badge/License-AGPL--3.0--or--later-blue.svg" alt="License: AGPL-3.0-or-later"></a>
<a href=".github/workflows/ci.yml"><img src="https://github.com/Aeshur/switchmut/actions/workflows/ci.yml/badge.svg" alt="Checks"></a>
</p>

## Portable setup

Extract `Switchmut-windows.zip` and run `Switchmut.exe`. The executable's folder
must be writable. Select a Gamepad and assign buttons on the Gamepad page.
Settings save automatically to `config-switchmut.json`. Diagnostics go to
`log-switchmut.log` beside the executable.

## Build

Install the pinned Rust toolchain through rustup and Visual Studio C++ Build
Tools with a Windows SDK:

```powershell
.\tools\package-windows.ps1 -Mode Headless
```

Use `-Mode Interactive` for the extracted native tray smoke check on a desktop.
Add `-NativeChecks` to run the native regression checks. The package ZIP
contains `Switchmut.exe` only.

## Acknowledgement

FFXI Switchmon

## License

<a href="LICENSE"><img src="https://www.gnu.org/graphics/agplv3-155x51.png" alt="GNU AGPLv3 logo"></a>
