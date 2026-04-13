# Developing The Quest 3 Fork

This repository is maintained as a **Quest 3 link-cable reverse tethering
fork** of gnirehtet. The active codebase is:

- the Android client in [`app/`](app/)
- the Java relay in [`relay-java/`](relay-java/)

The Rust relay has been removed from this fork.

## Tooling

- Android Studio with the Android SDK installed
- a Java 11 runtime for local Gradle builds
- `adb` from Android platform-tools

Android Studio is the recommended IDE for this repository. Plain IntelliJ is
not a good fit for the Android module and signing flow.

This project still uses Gradle 5.4.1 and Android Gradle Plugin 3.5.0. In
practice that means modern systems usually need **Java 11**, not the newest JDK
on the machine.

If your default `java` is too new, point `JAVA_HOME` at a Java 11 runtime
before running Gradle.

## Local Setup

Create a local `local.properties` file that points to your Android SDK:

```properties
sdk.dir=/path/to/Android/sdk
```

That file is local-only and should not be committed.

## Build

Use the Gradle wrapper:

```bash
./gradlew build
```

Important root tasks:

- `debugJava` builds the debug Android app and the Java relay
- `releaseJava` builds the release Android app and the Java relay
- `checkJava` runs checks for both modules
- `debugAll`, `releaseAll`, and `checkAll` are aliases for the Java-only fork

Useful focused builds:

```bash
./gradlew :relay-java:jar
./gradlew :app:assembleDebug
./gradlew :app:assembleRelease
./gradlew :relay-java:check
```

## Release Outputs

The release bundle is still built around the same files:

- `gnirehtet.apk`
- `gnirehtet.jar`
- `gnirehtet`
- `gnirehtet.cmd`
- `gnirehtet-run.cmd`

The packaging script is [`release`](release). It expects:

- a release APK from `app/build/outputs/apk/release/`
- the relay jar from `relay-java/build/libs/`
- the launcher scripts from `relay-java/scripts/`

## Release Signing

Public releases should use a **signed** Android release build.

Signing is configured through [`config/android-signing.gradle`](config/android-signing.gradle)
and activates only if these Gradle properties exist:

- `RELEASE_STORE_FILE`
- `RELEASE_STORE_PASSWORD`
- `RELEASE_KEY_ALIAS`
- `RELEASE_KEY_PASSWORD`

Without those properties, Gradle produces `gnirehtet-release-unsigned.apk`
instead of `gnirehtet-release.apk`, and the packaging script will not complete
as-is.

## Architecture

The Android client registers a [VPN], intercepts the headset's IPv4 traffic,
and forwards raw packets to the relay server over a TCP connection established
through `adb reverse`:

```bash
adb reverse localabstract:gnirehtet tcp:31416
```

The relay server receives those packets and opens normal TCP and UDP sockets on
the computer, effectively acting as a user-space NAT for the connected headset.

[VPN]: https://developer.android.com/reference/android/net/VpnService.html

## Main Components

Android-side code in [`app/`](app/):

- [`GnirehtetActivity`](app/src/main/java/com/genymobile/gnirehtet/GnirehtetActivity.java)
  receives the `START` and `STOP` intents
- [`GnirehtetService`](app/src/main/java/com/genymobile/gnirehtet/GnirehtetService.java)
  owns the VPN lifecycle
- [`RelayTunnel`](app/src/main/java/com/genymobile/gnirehtet/RelayTunnel.java)
  and [`PersistentRelayTunnel`](app/src/main/java/com/genymobile/gnirehtet/PersistentRelayTunnel.java)
  manage the host relay connection

Relay-side code in [`relay-java/`](relay-java/):

- [`Main`](relay-java/src/main/java/com/genymobile/gnirehtet/Main.java)
  implements the CLI commands and reconnect behavior
- [`Relay`](relay-java/src/main/java/com/genymobile/gnirehtet/relay/Relay.java)
  owns the selector loop
- [`Client`](relay-java/src/main/java/com/genymobile/gnirehtet/relay/Client.java)
  represents one connected Android client

![archi](assets/archi.png)

## Quest 3 Behavior Notes

- `run` in this fork is intended to recover better after Quest cable reconnects.
- Reconnect restores the tunnel, but it does not preserve existing TCP sessions.
- The transport is still VPN-over-ADB, so it is not a full Ethernet bridge or
  guaranteed same-LAN replacement for discovery-heavy apps.
