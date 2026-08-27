<p align="center">
  <img src="host-rust/crates/gnirehtet-vd/assets/tray-on.svg" width="96" alt="Quest VD Wired tray icon">
</p>

<h1 align="center">Quest VD Wired</h1>

<p align="center">
  <strong>Virtual Desktop over USB-C for Meta Quest 3</strong><br>
  Simple wired networking without relying on Quest Wi-Fi.
</p>

<p align="center">
  <a href="https://github.com/kkoemets/quest-vd-wired/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/kkoemets/quest-vd-wired?display_name=tag&amp;sort=semver"></a>
  <a href="LICENSE"><img alt="Apache 2.0 license" src="https://img.shields.io/github/license/kkoemets/quest-vd-wired"></a>
  <a href="https://github.com/kkoemets/quest-vd-wired"><img alt="GitHub stars" src="https://img.shields.io/github/stars/kkoemets/quest-vd-wired?style=flat&amp;logo=github"></a>
  <img alt="Windows 10 and 11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078D4?logo=windows11&amp;logoColor=white">
  <img alt="Meta Quest 3" src="https://img.shields.io/badge/Meta%20Quest-3-0467DF?logo=meta&amp;logoColor=white">
</p>

<p align="center">
  <a href="https://github.com/kkoemets/quest-vd-wired/releases/download/v4.1.8/quest-vd-wired-v4.1.8-windows-x64.zip"><strong>Download v4.1.8</strong></a>
  · <a href="#quick-start">Setup</a>
  · <a href="#troubleshooting">Troubleshooting</a>
  · <a href="https://github.com/kkoemets/quest-vd-wired/discussions/categories/q-a">Get help</a>
</p>

## What it does

- Carries Quest network traffic through a USB 3 cable instead of Quest Wi-Fi.
- Runs from the Windows tray and reconnects after cable interruptions.
- Includes **Diagnose and fix** and redacted local diagnostics.

It still requires Virtual Desktop on the Quest and Virtual Desktop Streamer on
the PC. It does not replace Meta Quest Link, remove video compression, raise
VD's bitrate limit, or provide HDMI/DisplayPort input.

## Requirements

- Windows 10 or 11 x64 with internet for first setup
- Meta Quest 3 with Developer Mode and USB debugging enabled
- USB 3 data or Quest Link cable
- Virtual Desktop on the Quest
- Virtual Desktop Streamer on the PC

Quest 2, Quest 3S, and Quest Pro have not been tested.

## Quick start

1. [Download v4.1.8](https://github.com/kkoemets/quest-vd-wired/releases/download/v4.1.8/quest-vd-wired-v4.1.8-windows-x64.zip) and extract it.
2. Connect and unlock the Quest. Accept **Allow USB debugging**.
3. Run `quest-vd-wired.exe`.
4. Accept the notification and VPN prompts inside the headset.
5. Start Virtual Desktop Streamer, then open Virtual Desktop on the Quest.

Green means **Wired link** is enabled or connecting. To verify the wired path,
turn off Quest Wi-Fi and confirm that Virtual Desktop can still reach the PC.

<p align="center">
  <img src="https://github.com/user-attachments/assets/2ce519de-3997-47e5-ac38-84eb974fd804" width="520" alt="Quest VD Wired tray menu">
</p>

## Cable basics

- Use a cable marked **USB 3**, **SuperSpeed**, or **Quest Link**.
- Connect directly to the PC while setting up. Avoid hubs and adapters.
- USB-A or USB-C works on the PC side if the port supports USB 3.
- Charging wattage and data speed are separate specifications.
- Secure the cable to the head strap to reduce stress on the Quest port.

## Troubleshooting

| Problem | Try this |
| --- | --- |
| Headset not found | Unlock it, accept USB debugging, then try another USB 3 port or cable. |
| VD cannot find the PC | Confirm the Streamer is running, restart VD, then use **Diagnose and fix**. |
| Stutters or latency spikes | Restart VD, test a lower bitrate, and note the time of any spike. |
| Cable was unplugged | Reconnect, unlock the headset, and approve USB debugging again if asked. |
| Want normal Quest networking | Turn off **Wired link** from the tray or exit the app. |

Export a redacted support log from PowerShell in the extracted app folder:

```powershell
.\quest-vd-wired.exe diagnostics export quest-vd-wired-support.jsonl
```

When asking for help, include your Windows version, cable, whether VD worked
with Quest Wi-Fi off, whether reconnect worked, and the exported log if useful.

- [Ask a question](https://github.com/kkoemets/quest-vd-wired/discussions/categories/q-a)
- [Report a reproducible issue](https://github.com/kkoemets/quest-vd-wired/issues)

## Common questions

### Is it faster or clearer than Wi-Fi?

Not necessarily. It removes Wi-Fi variability but keeps the same VD encoding,
decoding, and bitrate limits. A good dedicated router may perform just as well.

### Does Wi-Fi have to be off?

No. Turning it off is only the clearest test that VD is using the cable.

### Does it use Meta Quest Link?

No. It provides a network path for Virtual Desktop.

### What traffic goes through the cable?

While **Wired link** is on, Quest network traffic is routed through the PC. Turn
it off to restore the normal Quest network path.

## Privacy and license

Diagnostics stay on the PC, are redacted, and never include packet contents,
destination addresses, account details, or browsing history. Logs rotate at
about 200 MiB total and are uploaded only if you share an exported file.

Independent community project. Not affiliated with Meta or Virtual Desktop.
Licensed under the [Apache License 2.0](LICENSE).
