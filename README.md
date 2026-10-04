<h1 align="center">Switchmut</h1>

<p align="center">
<img src="assets/icon.png" alt="Switchmut logo" width="128">
</p>

<p align="center">
Switch between Final Fantasy XIV 1.23b clients on Windows with a gamepad.
</p>

<p align="center">
<a href="LICENSE"><img src="https://img.shields.io/badge/License-AGPL--3.0--or--later-blue.svg" alt="License: AGPL-3.0-or-later"></a>
<a href=".github/workflows/ci.yml"><img src="https://github.com/Aeshur/switchmut/actions/workflows/ci.yml/badge.svg" alt="Checks"></a>
</p>

## Portable setup

Extract `Switchmut-windows.zip` and run `Switchmut.exe`. The executable's folder
must be writable.

1. On the Gamepad page, select a controller or leave it set to Automatic.
2. Select an action, click Assign, then press the gamepad button to use.
3. Use Switch Next or Switch Previous to change clients. Hold the button assigned
   to Toggle Menu, move the stick to select an action, then release the button.

The General page lets you refresh the client list, change its order and choose
which clients are allowed. Client order and Allow / Deny choices are not saved
between runs. Controller, button and refresh settings save automatically to
`config-switchmut.json`. Diagnostics go to `log-switchmut.log` beside the
executable.

Closing the settings window leaves Switchmut running in the tray. Double-click
the tray icon to reopen settings, or right-click it and choose Exit to quit.

## Build

On Windows, install rustup, the toolchain pinned in `rust-toolchain.toml`, and
Visual Studio C++ Build Tools with a Windows SDK. Then run:

```powershell
.\tools\package-windows.ps1 -Mode Headless
```

This runs formatting, Clippy and tests, then builds and checks a release package
under `release/`. The package ZIP contains `Switchmut.exe` only.

Use `-Mode Interactive -NativeChecks` on an interactive Windows desktop to also
run the native regression tests and check the extracted application. Close any
running Switchmut instance first. Add `-LiveGameCheck` only when a supported game
client is running; it checks foreground switching, capture and minimize/restore.

## Acknowledgement

FFXI Switchmon

## License

<a href="LICENSE"><img src="https://www.gnu.org/graphics/agplv3-155x51.png" alt="GNU AGPLv3 logo"></a>
