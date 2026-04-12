# Gnirehtet for developers

## Requirements

You need the [Android SDK] (_Android Studio_) and a Java 8-compatible JDK.

[Android SDK]: https://developer.android.com/studio/index.html

## Build

If `gradle` is installed on your computer:

    gradle build

Otherwise, use the Gradle wrapper:

    ./gradlew build

The root project exposes Java-only tasks:

 - `debugJava` and `releaseJava` build the Android application and the Java relay server
 - `checkJava` runs the Android and Java relay checks
 - `debugAll`, `releaseAll`, and `checkAll` are aliases for the Java build path

To build just the Java relay jar:

    ./gradlew :relay-java:jar

## Android Studio

Import the repository into _Android Studio_ via File → Import….

From there, you can develop on the Android application in [`app/`](app/) and
the Java relay server in [`relay-java/`](relay-java/), run Gradle tasks, and
execute tests with IDE output.

## Overview

The Android client registers itself as a [VPN], intercepts the device's IPv4
traffic, and forwards raw packets to the relay server over a TCP connection.
That TCP connection is established through `adb reverse`:

    adb reverse localabstract:gnirehtet tcp:31416

The relay server receives those packets and opens normal TCP/UDP sockets on the
computer, effectively acting as a user-space NAT for the connected Android
device.

[VPN]: https://developer.android.com/reference/android/net/VpnService.html

## Main components

Client-side Android code lives under [`app/`](app/):

 - [`GnirehtetActivity`](app/src/main/java/com/genymobile/gnirehtet/GnirehtetActivity.java)
   receives the `START` and `STOP` intents
 - [`GnirehtetService`](app/src/main/java/com/genymobile/gnirehtet/GnirehtetService.java)
   owns the VPN lifecycle
 - [`RelayTunnel`](app/src/main/java/com/genymobile/gnirehtet/RelayTunnel.java)
   and [`PersistentRelayTunnel`](app/src/main/java/com/genymobile/gnirehtet/PersistentRelayTunnel.java)
   manage the connection to the host relay

Relay-side Java code lives under [`relay-java/`](relay-java/):

 - [`Main`](relay-java/src/main/java/com/genymobile/gnirehtet/Main.java)
   implements the CLI commands
 - [`Relay`](relay-java/src/main/java/com/genymobile/gnirehtet/relay/Relay.java)
   owns the selector loop
 - [`Client`](relay-java/src/main/java/com/genymobile/gnirehtet/relay/Client.java)
   represents one connected Android client

![archi](assets/archi.png)
