# Gnirehtet Quest 3 Fork (v3.0.0)

This fork packages **gnirehtet** as a practical reverse-tethering tool for
**Meta Quest 3 over Link cable / USB**. The goal is simple: let the headset use
the host computer's internet connection through `adb`, with Quest 3-focused
reconnect and startup fixes on top of the original project.

This is still the same core model:

- the Android app creates a VPN on the headset
- the headset sends IPv4 traffic through `adb reverse`
- the Java relay on the computer opens real TCP and UDP sockets on the host

It does **not** require root on the headset or on the computer.

## Scope

This repository is now maintained as a **Quest 3 link-cable reverse tethering
fork**. It is intentionally focused on:

- Quest 3 setup over USB / Link cable
- the Android app in [`app/`](app/)
- the Java relay in [`relay-java/`](relay-java/)
- release packaging that ships the APK, the relay jar, and the launcher scripts

It no longer ships the Rust relay.

## What This Is Not

This project is useful for getting Quest 3 online over cable, but it is still
not a true Ethernet bridge or same-LAN replacement.

- It tunnels IPv4 traffic, not IPv6.
- It uses Android `VpnService`, so some apps that expect real local network
  behavior may still behave differently from Wi-Fi.
- If the cable or `adb` connection drops, live TCP sessions are reset and must
  reconnect. The fork improves recovery, but it does not preserve in-flight
  sessions.

## Requirements

- Meta Quest 3 with Developer Mode enabled
- USB debugging enabled on the headset
- a working USB / Link cable connection to the host computer
- recent [Android platform-tools / `adb`][platform-tools]
- Java on the host for running `gnirehtet.jar`

The Android application still targets API 21+.

On Windows, if you only need `adb` for this project, download the
[platform-tools archive][platform-tools-windows] and place these files next to
the release bundle:

- `adb.exe`
- `AdbWinApi.dll`
- `AdbWinUsbApi.dll`

The first connection requires approving the USB debugging fingerprint prompt in
the headset.

[platform-tools]: https://developer.android.com/studio/releases/platform-tools
[platform-tools-windows]: https://dl.google.com/android/repository/platform-tools-latest-windows.zip

## Releases

Download releases from this fork's releases page:

- [Latest release](https://github.com/kkoemets/gnirehtet/releases/latest)

The release archive keeps the same simple layout:

- `gnirehtet.apk`
- `gnirehtet.jar`
- `gnirehtet`
- `gnirehtet.cmd`
- `gnirehtet-run.cmd`

For this fork, the intended public release line starts at `v3.0.0`.

## Quick Start For Quest 3

On macOS and Linux:

```bash
adb install -r gnirehtet.apk
./gnirehtet run
```

On Windows:

1. Install `gnirehtet.apk` once with `adb install -r gnirehtet.apk`.
2. Double-click `gnirehtet-run.cmd`, or run `gnirehtet run` in a terminal.

What to expect:

- the first launch asks the headset to allow the VPN connection
- a key icon appears while gnirehtet is active
- `run` keeps the relay open in the current terminal
- if the headset disconnects and reconnects, this fork attempts to restore the
  client automatically when `adb` comes back

The original permission UI still looks like this:

![request](assets/request.jpg)

When active, Android shows the VPN key icon:

![key](assets/key.png)

## Commands

The high-level entry point is still the `gnirehtet` CLI. On Windows, replace
`./gnirehtet` with `gnirehtet`.

Start reverse tethering for one connected device:

```bash
./gnirehtet run
```

Start the relay only:

```bash
./gnirehtet relay
```

Install the APK:

```bash
./gnirehtet install [serial]
```

Start the client without running the relay:

```bash
./gnirehtet start [serial]
```

Stop the client:

```bash
./gnirehtet stop [serial]
```

Reset the `adb reverse` tunnel:

```bash
./gnirehtet tunnel [serial]
```

Monitor future device connections and auto-start them:

```bash
./gnirehtet autorun
```

If `adb devices` lists more than one device, pass the serial explicitly.

## Manual Commands

If you prefer to drive the lower-level pieces yourself:

Start the relay:

```bash
./gnirehtet relay
```

Install the APK:

```bash
adb install -r gnirehtet.apk
```

Set up the tunnel and start the Quest client:

```bash
adb reverse localabstract:gnirehtet tcp:31416
adb shell am start -a com.genymobile.gnirehtet.START \
    -n com.genymobile.gnirehtet/.GnirehtetActivity
```

Stop the client:

```bash
adb shell am start -a com.genymobile.gnirehtet.STOP \
    -n com.genymobile.gnirehtet/.GnirehtetActivity
```

## Quest 3 Notes

- This fork improves reconnect behavior for `run`, but a reconnect still
  rebuilds the tunnel instead of resuming existing flows.
- If the headset asks again for USB debugging authorization after reconnect,
  approve it before expecting the tunnel to recover.
- Some apps may still be sensitive to discovery or same-LAN assumptions because
  this is a VPN-over-ADB path, not a native Ethernet device.

## Environment Variables

Use a custom `adb` binary:

```bash
ADB=/path/to/adb ./gnirehtet run
```

Use a custom APK path:

```bash
GNIREHTET_APK=/path/to/gnirehtet.apk ./gnirehtet run
```

## Developers

See [DEVELOP.md](DEVELOP.md) for build, release, and architecture details for
this fork.

## Upstream

This fork is based on the original Genymobile project and keeps the same
package name and core architecture. The fork exists because the original
project is effectively dormant while Quest 3 users still need a working wired
reverse-tethering path.

Original project articles:

- [Introducing “gnirehtet”, a reverse tethering tool for Android][medium-1]
- [French version][blog-1]

[medium-1]: https://medium.com/@rom1v/gnirehtet-reverse-tethering-android-2afacdbdaec7
[blog-1]: https://blog.rom1v.com/2017/03/gnirehtet/

## License

    Copyright (C) 2017 Genymobile

    Licensed under the Apache License, Version 2.0 (the "License");
    you may not use this file except in compliance with the License.
    You may obtain a copy of the License at

        http://www.apache.org/licenses/LICENSE-2.0

    Unless required by applicable law or agreed to in writing, software
    distributed under the License is distributed on an "AS IS" BASIS,
    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
    See the License for the specific language governing permissions and
    limitations under the License.
