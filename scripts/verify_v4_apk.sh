#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: verify_v4_apk.sh APK EXPECTED_CERT_SHA256" >&2
    exit 2
fi

apk="$1"
expected_cert="$(printf '%s' "$2" | tr -d ':[:space:]' | tr '[:upper:]' '[:lower:]')"
if [[ ! -f "$apk" ]]; then
    echo "signed APK is missing" >&2
    exit 1
fi
if [[ ! "$expected_cert" =~ ^[0-9a-f]{64}$ ]]; then
    echo "expected signing certificate SHA-256 is missing or malformed" >&2
    exit 1
fi

sdk_root="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-}}"
if [[ -z "$sdk_root" ]]; then
    echo "ANDROID_SDK_ROOT or ANDROID_HOME is required" >&2
    exit 1
fi
build_tools="${ANDROID_BUILD_TOOLS_VERSION:-36.0.0}"
apksigner="$sdk_root/build-tools/$build_tools/apksigner"
apkanalyzer="$sdk_root/cmdline-tools/latest/bin/apkanalyzer"
if [[ ! -x "$apksigner" || ! -x "$apkanalyzer" ]]; then
    echo "Android build-tools $build_tools or cmdline-tools latest are incomplete" >&2
    exit 1
fi

verification="$(mktemp)"
listing="$(mktemp)"
packaged_notices="$(mktemp)"
trap 'rm -f "$verification" "$listing" "$packaged_notices"' EXIT
"$apksigner" verify --verbose --print-certs "$apk" >"$verification"

signer_digests="$(
    sed -n 's/^Signer #[0-9][0-9]* certificate SHA-256 digest: //p' "$verification" \
        | tr -d ':[:space:]' \
        | tr '[:upper:]' '[:lower:]'
)"
if [[ "$signer_digests" != "$expected_cert" ]]; then
    echo "APK signing certificate does not match the expected public identity" >&2
    exit 1
fi

package_name="$("$apkanalyzer" manifest application-id "$apk")"
if [[ "$package_name" != "com.genymobile.gnirehtet" ]]; then
    echo "APK application identity is not com.genymobile.gnirehtet" >&2
    exit 1
fi
version_code="$("$apkanalyzer" manifest version-code "$apk")"
version_name="$("$apkanalyzer" manifest version-name "$apk")"
min_sdk="$("$apkanalyzer" manifest min-sdk "$apk")"
target_sdk="$("$apkanalyzer" manifest target-sdk "$apk")"
debuggable="$("$apkanalyzer" manifest debuggable "$apk" | tr '[:upper:]' '[:lower:]')"
if [[ "$version_code" != "72" || "$version_name" != "4.1.34" ]]; then
    echo "APK version is not the exact v4.1.34 helper identity" >&2
    exit 1
fi
if [[ "$debuggable" != "false" ]]; then
    echo "APK release is debuggable" >&2
    exit 1
fi
if [[ "$min_sdk" != "29" || "$target_sdk" != "36" ]]; then
    echo "APK SDK bounds are not the exact Quest v4 product contract" >&2
    exit 1
fi

unzip -Z1 "$apk" >"$listing"
native_abis="$(
    sed -n 's#^lib/\([^/]*\)/.*\.so$#\1#p' "$listing" \
        | LC_ALL=C sort -u
)"
if [[ "$native_abis" != "arm64-v8a" ]]; then
    echo "APK native ABI set is not exactly arm64-v8a" >&2
    exit 1
fi
if ! grep -q '^lib/arm64-v8a/libhev-socks5-tunnel\.so$' "$listing"; then
    echo "APK does not contain the selected HEV native engine" >&2
    exit 1
fi
if ! grep -q '^assets/THIRD_PARTY_NOTICES\.md$' "$listing"; then
    echo "APK omits required third-party notices" >&2
    exit 1
fi
unzip -p "$apk" assets/THIRD_PARTY_NOTICES.md >"$packaged_notices"
repo_root="$(cd "$(dirname "$0")/.." && pwd)"
if ! cmp -s "$packaged_notices" "$repo_root/android-v4/app/src/main/assets/THIRD_PARTY_NOTICES.md"; then
    echo "packaged third-party notices differ from the reviewed source" >&2
    exit 1
fi
for required_notice in \
    'org.jetbrains.kotlin:kotlin-stdlib:2.2.10' \
    'org.jetbrains:annotations:13.0' \
    'TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION'; do
    if ! grep -Fq "$required_notice" "$packaged_notices"; then
        echo "packaged third-party notices omit required Android runtime license data" >&2
        exit 1
    fi
done
