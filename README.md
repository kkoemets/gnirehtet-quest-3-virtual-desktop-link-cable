# Gnirehtet For Quest 3 Link Cable And Virtual Desktop (v3.0.0)

This project packages **gnirehtet** as a practical reverse-tethering tool for
**Meta Quest 3 over Link cable / USB**. It is especially useful for
**Virtual Desktop on Quest 3** when you want a wired PCVR setup without
depending on Wi-Fi. The goal is simple: let the headset use the host
computer's internet connection through `adb`, with Quest 3-focused reconnect
and startup fixes.

This is still the same core model:

- the Android app creates a VPN on the headset
- the headset sends IPv4 traffic through `adb reverse`
- the Java relay on the computer opens real TCP and UDP sockets on the host

It does **not** require root on the headset or on the computer.

## Quest 3 Start Here

If you only want the simple Quest 3 setup, read this section and ignore the
advanced reference lower down.

### What you need

- Meta Quest 3 with Developer Mode enabled
- USB debugging enabled on the headset
- a working USB / Link cable connection to the host computer
- recent [Android platform-tools / `adb`][platform-tools]
- Java on the host for running Gnirehtet

`adb` is mandatory for this project. Gnirehtet uses it to talk to the Quest,
install the APK, create the `adb reverse` tunnel, start the client, stop the
client, and recover after reconnects. Without platform-tools / `adb`, Gnirehtet
cannot work.

If `java -version` works on your computer, that is enough for the release
bundle.

The first connection requires approving the USB debugging fingerprint prompt in
the headset.

### What you do not need

- SideQuest for the normal setup path
- a manual APK install on a fresh install

SideQuest is not required because it is only another way to sideload APKs over
`adb`. Gnirehtet already uses `adb` directly, and `gnirehtet run` installs the
app automatically when needed and then starts it for you.

On Windows, if you only need `adb` for this project, download the
[platform-tools archive][platform-tools-windows] and either keep `adb` in your
PATH or place these files next to Gnirehtet:

- `adb.exe`
- `AdbWinApi.dll`
- `AdbWinUsbApi.dll`

The release bundle also includes `gnirehtet-get-adb.cmd`, which downloads
Google's official Windows platform-tools into the same folder for you.

It also includes a small `README.txt` in the zip for the basic Quest 3 setup
flow.

### Fast setup

1. Download the [latest release](https://github.com/kkoemets/gnirehtet/releases/latest) and extract it.
2. Connect the Quest 3 by cable.
3. Put on the headset and accept the USB debugging prompt.
4. Start Gnirehtet.

On macOS and Linux:

```bash
./gnirehtet run
```

On Windows:

- if `adb` is not already in your PATH and not already next to Gnirehtet,
  double-click `gnirehtet-get-adb.cmd` once
- double-click `gnirehtet-run.cmd`
- or run `gnirehtet run` in a terminal

On a fresh install, `run` installs the app automatically if it is missing. If
you already had another Gnirehtet build installed on the headset and the first
run fails to install, remove the old app once:

```bash
adb uninstall com.genymobile.gnirehtet
```

### What success looks like

- the first launch asks the headset to allow the VPN connection
- a key icon appears while Gnirehtet is active
- `run` keeps the relay open in the current terminal or command window
- with Quest Wi-Fi turned off, the headset still has internet access

The original permission UI still looks like this:

![request](assets/request.jpg)

When active, Android shows the VPN key icon:

![key](assets/key.png)

### Virtual Desktop order

1. Start Virtual Desktop Streamer on the PC first.
2. Start Gnirehtet and approve the Quest prompts.
3. Turn off Quest Wi-Fi if you want the cable-only path.
4. Open Virtual Desktop on the headset.

Community reports show that Virtual Desktop's network wording can be confusing
on this setup. The more reliable signals are the Gnirehtet VPN prompt, the key
icon, and the fact that the headset still has internet access with Wi-Fi
disabled.

### Common problems

- The download looks incomplete: download the latest release again from GitHub.
- The window only shows `Starting relay server...`: `adb` often does not yet
  see an authorized Quest. Run `adb devices`, put on the headset, accept the
  USB debugging prompt, then try again.
- Windows says `adb` is not found: run `gnirehtet-get-adb.cmd` from the release
  folder, then retry `gnirehtet-run.cmd`.
- `adb devices` shows more than one device: pass the serial explicitly, for
  example `./gnirehtet run 1WMHH...`.
- The first install fails with a signature mismatch: uninstall any older
  Gnirehtet app from the headset once, then retry.
- The cable was unplugged and plugged back in: Gnirehtet attempts to recover
  when `adb` comes back, but live TCP sessions are reset. If the headset asks
  again for USB debugging authorization, approve it before expecting recovery.
- Virtual Desktop cannot see the PC at all: confirm that Virtual Desktop
  Streamer is already running on the PC before you launch Virtual Desktop on
  the headset.

Most Quest 3 users can stop reading here.

<details>
<summary>Advanced CLI reference</summary>

### Commands

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

### Manual commands

If you prefer to drive the lower-level pieces yourself:

Start the relay:

```bash
./gnirehtet relay
```

Install the APK:

If you need to remove an older Gnirehtet app first:

```bash
adb uninstall com.genymobile.gnirehtet
```

Then install the app:

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

### Environment variables

Use a custom `adb` binary:

```bash
ADB=/path/to/adb ./gnirehtet run
```

Use a custom APK path:

```bash
GNIREHTET_APK=/path/to/gnirehtet.apk ./gnirehtet run
```

</details>

<details>
<summary>Project scope and limits</summary>

### What this is not

This project is useful for getting Quest 3 online over cable, but it is still
not a true Ethernet bridge or same-LAN replacement.

- It tunnels IPv4 traffic, not IPv6.
- It uses Android `VpnService`, so some apps that expect real local network
  behavior may still behave differently from Wi-Fi.
- If the cable or `adb` connection drops, live TCP sessions are reset and must
  reconnect. The reconnect path does not preserve in-flight sessions.

### Quest 3 notes

- `run` is intended to recover better after reconnects, but a reconnect still
  rebuilds the tunnel instead of resuming existing flows.
- If the headset asks again for USB debugging authorization after reconnect,
  approve it before expecting the tunnel to recover.
- Some apps may still be sensitive to discovery or same-LAN assumptions because
  this is a VPN-over-ADB path, not a native Ethernet device.

### Developers

See [DEVELOP.md](DEVELOP.md) for build, release, and architecture details for
this project.

</details>

[platform-tools]: https://developer.android.com/studio/releases/platform-tools
[platform-tools-windows]: https://dl.google.com/android/repository/platform-tools-latest-windows.zip

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
