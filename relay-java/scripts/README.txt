Gnirehtet For Quest 3 Link Cable And Virtual Desktop

Quick start

1. Make sure Meta Developer Mode is enabled for your Quest 3.
2. Connect the Quest 3 to the PC by USB / Link cable.
3. Put on the headset and accept the USB debugging prompt.
4. On Windows, if adb is not already installed, run gnirehtet-get-adb.cmd once.
5. If you previously installed another Gnirehtet APK and install fails, run:
   adb uninstall com.genymobile.gnirehtet
6. Start Gnirehtet:
   - Windows: run gnirehtet-run.cmd
   - macOS / Linux: run ./gnirehtet run

What to expect

- The first launch asks the headset to allow the VPN connection.
- With Quest Wi-Fi turned off, the headset should still have internet access.

Notes

- gnirehtet-get-adb.cmd downloads Google's official Android platform-tools
  into this folder. It does not ship adb in the release zip.
- If the cable is unplugged and plugged back in, approve the USB debugging
  prompt again if the headset asks for it.

More help

- Full project guide: README.md in the repository
- Developer/build notes: DEVELOP.md in the repository
