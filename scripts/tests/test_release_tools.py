from __future__ import annotations

import os
import re
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock


SCRIPTS = Path(__file__).resolve().parents[1]
REPOSITORY = SCRIPTS.parent
sys.path.insert(0, str(SCRIPTS))

import generate_rust_notices  # noqa: E402
import generate_v4_native_sbom  # noqa: E402
import normalize_windows_pe  # noqa: E402
import verify_windows_release  # noqa: E402


class WindowsPeNormalizationTest(unittest.TestCase):
    @staticmethod
    def pe_fixture(marker: int) -> bytes:
        data = bytearray(0x600)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 0x3C, 0x80)
        pe = 0x80
        data[pe : pe + 4] = b"PE\0\0"
        struct.pack_into("<H", data, pe + 4, 0x8664)
        struct.pack_into("<H", data, pe + 6, 1)
        struct.pack_into("<I", data, pe + 8, marker)
        struct.pack_into("<H", data, pe + 20, 0xF0)
        optional = pe + 24
        struct.pack_into("<H", data, optional, 0x20B)
        struct.pack_into("<I", data, optional + 108, 16)
        struct.pack_into("<II", data, optional + 112 + 6 * 8, 0x1100, 56)
        section = optional + 0xF0
        data[section : section + 8] = b".rdata\0\0"
        struct.pack_into("<IIII", data, section + 8, 0x400, 0x1000, 0x400, 0x200)
        debug = 0x300
        struct.pack_into("<I", data, debug + 4, marker)
        struct.pack_into("<I", data, debug + 12, 2)
        struct.pack_into("<I", data, debug + 16, 32)
        struct.pack_into("<I", data, debug + 24, 0x400)
        struct.pack_into("<I", data, debug + 28 + 4, marker)
        data[0x400:0x404] = b"RSDS"
        data[0x404:0x414] = bytes([marker & 0xFF]) * 16
        struct.pack_into("<I", data, 0x414, 1)
        data[0x418:0x420] = b"test.pdb"
        return bytes(data)

    def test_normalization_is_idempotent_and_removes_build_variance(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "first.exe"
            second = Path(directory) / "second.exe"
            first.write_bytes(self.pe_fixture(0x11111111))
            second.write_bytes(self.pe_fixture(0x22222222))

            self.assertEqual((2, 1), normalize_windows_pe.normalize(first))
            self.assertEqual((2, 1), normalize_windows_pe.normalize(second))
            normalized = first.read_bytes()
            self.assertEqual(normalized, second.read_bytes())
            self.assertEqual((2, 1), normalize_windows_pe.normalize(first))
            self.assertEqual(normalized, first.read_bytes())


class WindowsReleaseVerificationTest(unittest.TestCase):
    def test_local_build_paths_are_rejected_in_ascii_and_utf16(self) -> None:
        examples = (
            b"MZpayload/Users/example/project/src/main.rs\0",
            b"MZpayloadC:\\Users\\example\\project\\src\\main.rs\0",
            b"MZpayload/private/var/folders/example/build\0",
            b"MZ" + "C:\\Users\\example\\project\\src\\main.rs".encode("utf-16-le"),
        )
        for payload in examples:
            with self.subTest(payload=payload[:32]):
                with tempfile.TemporaryDirectory() as directory:
                    executable = Path(directory) / "release.exe"
                    executable.write_bytes(payload)
                    with self.assertRaises(verify_windows_release.VerificationError):
                        verify_windows_release.verify_no_local_paths(executable)

    def test_explicit_nonstandard_local_root_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "release.exe"
            executable.write_bytes(b"MZpayloadD:\\build-agent\\private\\source\\main.rs\0")
            with self.assertRaises(verify_windows_release.VerificationError):
                verify_windows_release.verify_no_local_paths(
                    executable,
                    [r"D:\build-agent\private"],
                )

    def test_safe_binary_and_system_imports_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "release.exe"
            executable.write_bytes(b"MZpayload/source/quest-vd-wired/src/main.rs\0")
            verify_windows_release.verify_no_local_paths(executable)
            completed = subprocess.CompletedProcess(
                args=[],
                returncode=0,
                stdout="Import {\n  Name: KERNEL32.dll\n}\n",
                stderr="",
            )
            with mock.patch.object(
                verify_windows_release.subprocess,
                "run",
                return_value=completed,
            ):
                self.assertEqual(
                    ["KERNEL32.dll"],
                    verify_windows_release.verify_static_runtime_imports(
                        executable,
                        Path("llvm-readobj"),
                    ),
                )

    def test_windows_version_metadata_is_required_exactly(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "release.exe"
            metadata = "".join(
                (
                    "ProductName\0Quest VD Wired\0",
                    "OriginalFilename\0quest-vd-wired.exe\0",
                    "ProductVersion\0" "4.1.0\0",
                )
            ).encode("utf-16-le")
            executable.write_bytes(b"MZ" + metadata)
            verify_windows_release.verify_version_metadata(
                executable,
                product_name="Quest VD Wired",
                original_filename="quest-vd-wired.exe",
                product_version="4.1.0",
            )
            with self.assertRaises(verify_windows_release.VerificationError):
                verify_windows_release.verify_version_metadata(
                    executable,
                    product_name="Quest VD Wired",
                    original_filename="gnirehtet-vd.exe",
                    product_version="4.1.0",
                )

    def test_windows_version_metadata_rejects_loose_or_wrong_field_strings(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "release.exe"
            metadata = "".join(
                (
                    "FileDescription\0Quest VD Wired\0",
                    "InternalName\0quest-vd-wired.exe\0",
                    "FileVersion\0" "4.1.0\0",
                    "ProductName\0Wrong product\0",
                    "OriginalFilename\0wrong.exe\0",
                    "ProductVersion\0" "4.1.0.0\0",
                )
            ).encode("utf-16-le")
            executable.write_bytes(b"MZ" + metadata)
            with self.assertRaises(verify_windows_release.VerificationError):
                verify_windows_release.verify_version_metadata(
                    executable,
                    product_name="Quest VD Wired",
                    original_filename="quest-vd-wired.exe",
                    product_version="4.1.0",
                )

    def test_dynamic_vc_runtime_import_is_rejected(self) -> None:
        for runtime in ("VCRUNTIME140.dll", "ucrtbase.dll", "api-ms-win-crt-runtime-l1-1-0.dll"):
            with self.subTest(runtime=runtime):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    executable = root / "release.exe"
                    executable.write_bytes(b"MZpayload")
                    completed = subprocess.CompletedProcess(
                        args=[],
                        returncode=0,
                        stdout=f"Import {{\n  Name: {runtime}\n}}\n",
                        stderr="",
                    )
                    with mock.patch.object(
                        verify_windows_release.subprocess,
                        "run",
                        return_value=completed,
                    ):
                        with self.assertRaises(verify_windows_release.VerificationError):
                            verify_windows_release.verify_static_runtime_imports(
                                executable,
                                Path("llvm-readobj"),
                            )


class NativeSbomTest(unittest.TestCase):
    def test_bom_is_deterministic_and_has_closed_graph(self) -> None:
        first = generate_v4_native_sbom.build_bom("4.0.0-beta.1", 1_700_000_000, "a" * 64, "b" * 64)
        second = generate_v4_native_sbom.build_bom("4.0.0-beta.1", 1_700_000_000, "a" * 64, "b" * 64)
        self.assertEqual(first, second)
        self.assertEqual("CycloneDX", first["bomFormat"])
        self.assertEqual("1.6", first["specVersion"])
        self.assertEqual(7, len(first["components"]))
        root = first["metadata"]["component"]
        self.assertEqual("com.genymobile", root["group"])
        self.assertEqual("a" * 64, root["hashes"][0]["content"])
        dependencies = {entry["ref"]: entry["dependsOn"] for entry in first["dependencies"]}
        dependency_refs = set(dependencies)
        self.assertIn(root["bom-ref"], dependency_refs)
        kotlin_ref = "pkg:maven/org.jetbrains.kotlin/kotlin-stdlib@2.2.10"
        annotations_ref = "pkg:maven/org.jetbrains/annotations@13.0"
        components = {entry["bom-ref"]: entry for entry in first["components"]}
        self.assertEqual("Apache-2.0", components[kotlin_ref]["licenses"][0]["license"]["id"])
        self.assertEqual("Apache-2.0", components[annotations_ref]["licenses"][0]["license"]["id"])
        self.assertIn(kotlin_ref, dependencies[root["bom-ref"]])
        self.assertEqual([annotations_ref], dependencies[kotlin_ref])
        self.assertEqual([], dependencies[annotations_ref])

    def test_runtime_classpath_report_requires_exact_sorted_graph(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / generate_v4_native_sbom.RUNTIME_CLASSPATH_REPORT
            report.parent.mkdir(parents=True)
            expected = sorted(generate_v4_native_sbom.EXPECTED_JVM_COMPONENTS)
            report.write_text("\n".join(expected) + "\n", encoding="utf-8")
            self.assertEqual(expected, generate_v4_native_sbom.verify_runtime_classpath_report(root))

            report.write_text(
                "\n".join(expected + ["invalid.example:unexpected-runtime:1"]) + "\n",
                encoding="utf-8",
            )
            with self.assertRaises(generate_v4_native_sbom.VerificationError):
                generate_v4_native_sbom.verify_runtime_classpath_report(root)

    def test_project_patch_set_covers_the_pinned_hev_changes(self) -> None:
        self.assertEqual(
            {
                "hev-lifecycle.patch": ".",
                "hev-split-udp-port.patch": ".",
                "hev-timeout-phases.patch": ".",
            },
            generate_v4_native_sbom.PROJECT_PATCH_SCOPES,
        )

    def test_patch_scope_verifier_rejects_extra_changes_in_a_patched_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory) / "checkout"
            checkout.mkdir()
            subprocess.run(["git", "init", "--quiet", str(checkout)], check=True)
            subprocess.run(
                ["git", "-C", str(checkout), "config", "user.email", "test@example.invalid"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", str(checkout), "config", "user.name", "Test"],
                check=True,
            )
            source = checkout / "src/value.c"
            source.parent.mkdir(parents=True)
            source.write_text("old\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(checkout), "add", "src/value.c"], check=True)
            subprocess.run(
                ["git", "-C", str(checkout), "commit", "--quiet", "-m", "fixture"],
                check=True,
            )
            patch = Path(directory) / "change.patch"
            patch.write_text(
                "diff --git a/src/value.c b/src/value.c\n"
                "--- a/src/value.c\n"
                "+++ b/src/value.c\n"
                "@@ -1 +1 @@\n"
                "-old\n"
                "+new\n",
                encoding="utf-8",
            )
            subprocess.run(["git", "-C", str(checkout), "apply", str(patch)], check=True)
            generate_v4_native_sbom.verify_patch_scope(checkout, [patch], "fixture")

            source.write_text("new\nextra\n", encoding="utf-8")
            with self.assertRaises(generate_v4_native_sbom.VerificationError):
                generate_v4_native_sbom.verify_patch_scope(checkout, [patch], "fixture")


class RustNoticeTest(unittest.TestCase):
    def test_notice_contains_resolved_license_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            package_dir = Path(directory) / "dependency"
            package_dir.mkdir()
            manifest = package_dir / "Cargo.toml"
            manifest.write_text("[package]\nname='example'\nversion='1.0.0'\n", encoding="utf-8")
            (package_dir / "LICENSE-MIT").write_text("sample <license>", encoding="utf-8")
            package_id = "registry+https://example.invalid#index/example@1.0.0"
            metadata = {
                "packages": [
                    {
                        "id": package_id,
                        "name": "example",
                        "version": "1.0.0",
                        "source": "registry+https://example.invalid/index",
                        "license": "MIT",
                        "license_file": None,
                        "manifest_path": str(manifest),
                    }
                ],
                "resolve": {"nodes": [{"id": package_id}]},
            }
            output = generate_rust_notices.render_notices(metadata)
            self.assertIn("## example 1.0.0", output)
            self.assertIn("sample &lt;license&gt;", output)

    def test_notice_records_metadata_when_crate_omits_license_text(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "Cargo.toml"
            manifest.write_text("", encoding="utf-8")
            package_id = "registry+https://example.invalid#index/example@1.0.0"
            metadata = {
                "packages": [
                    {
                        "id": package_id,
                        "name": "example",
                        "version": "1.0.0",
                        "source": "registry+https://example.invalid/index",
                        "license": "MIT",
                        "license_file": None,
                        "manifest_path": str(manifest),
                    }
                ],
                "resolve": {"nodes": [{"id": package_id}]},
            }
            output = generate_rust_notices.render_notices(metadata)
            self.assertIn("does not contain a standalone license file", output)


class CommandLineToolsTest(unittest.TestCase):
    def test_android_release_builder_fails_closed_without_signing_secrets(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            environment = os.environ.copy()
            for name in (
                "ANDROID_RELEASE_KEYSTORE_BASE64",
                "ANDROID_RELEASE_CERT_SHA256",
                "ORG_GRADLE_PROJECT_RELEASE_STORE_PASSWORD",
                "ORG_GRADLE_PROJECT_RELEASE_KEY_ALIAS",
                "ORG_GRADLE_PROJECT_RELEASE_KEY_PASSWORD",
            ):
                environment.pop(name, None)
            process = subprocess.run(
                ["bash", str(SCRIPTS / "build_v4_android_rc.sh"), directory],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )
            self.assertNotEqual(0, process.returncode)
            self.assertIn("required release input is missing", process.stderr)

    def test_checksum_writer_is_sorted_and_self_excluding(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "z.txt").write_text("z", encoding="utf-8")
            (root / "a.txt").write_text("a", encoding="utf-8")
            output = root / "SHA256SUMS"
            subprocess.run(
                [sys.executable, str(SCRIPTS / "write_sha256.py"), str(root), "--output", str(output)],
                check=True,
            )
            names = [line.split("  ", 1)[1] for line in output.read_text(encoding="utf-8").splitlines()]
            self.assertEqual(["a.txt", "z.txt"], names)

    def test_embedded_apk_requires_one_exact_copy(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            apk = root / "app.apk"
            executable = root / "host.exe"
            apk.write_bytes(b"PK\x03\x04signed-apk")
            executable.write_bytes(b"prefix" + apk.read_bytes() + b"suffix")
            subprocess.run(
                [sys.executable, str(SCRIPTS / "verify_embedded_apk.py"), str(executable), str(apk)],
                check=True,
            )

    def test_apk_payload_comparison_excludes_signing_block_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = root / "first.apk"
            second = root / "second.apk"
            with zipfile.ZipFile(first, "w") as archive:
                archive.writestr("classes.dex", b"payload")
            original = first.read_bytes()
            eocd = original.rfind(b"PK\x05\x06")
            central_offset = struct.unpack_from("<I", original, eocd + 16)[0]
            signing_bytes = b"different-signing-block"
            modified = bytearray(original[:central_offset] + signing_bytes + original[central_offset:])
            struct.pack_into("<I", modified, eocd + len(signing_bytes) + 16, central_offset + len(signing_bytes))
            second.write_bytes(modified)
            subprocess.run(
                [
                    sys.executable,
                    str(SCRIPTS / "compare_apk_payload.py"),
                    str(first),
                    str(second),
                ],
                check=True,
            )

    def test_apk_verifier_checks_exact_release_identity_and_notices(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk = root / "sdk"
            build_tools = sdk / "build-tools/36.0.0"
            command_tools = sdk / "cmdline-tools/latest/bin"
            build_tools.mkdir(parents=True)
            command_tools.mkdir(parents=True)
            digest = "0" * 64
            apksigner = build_tools / "apksigner"
            apksigner.write_text(
                f"#!/bin/sh\necho 'Signer #1 certificate SHA-256 digest: {digest}'\n",
                encoding="utf-8",
            )
            apkanalyzer = command_tools / "apkanalyzer"
            apkanalyzer.write_text(
                "#!/bin/sh\n"
                "case \"$2\" in\n"
                "application-id) echo com.genymobile.gnirehtet ;;\n"
                "version-code) echo 56 ;;\n"
                "version-name) echo 4.1.4 ;;\n"
                "min-sdk) echo 29 ;;\n"
                "target-sdk) echo 36 ;;\n"
                "debuggable) echo false ;;\n"
                "*) exit 2 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            apksigner.chmod(0o755)
            apkanalyzer.chmod(0o755)
            apk = root / "release.apk"
            notices = (SCRIPTS.parent / "android-v4/app/src/main/assets/THIRD_PARTY_NOTICES.md").read_bytes()
            with zipfile.ZipFile(apk, "w") as archive:
                archive.writestr("lib/arm64-v8a/libhev-socks5-tunnel.so", b"native")
                archive.writestr("assets/THIRD_PARTY_NOTICES.md", notices)
            environment = os.environ.copy()
            environment["ANDROID_HOME"] = str(sdk)
            environment["ANDROID_SDK_ROOT"] = str(sdk)
            subprocess.run(
                ["bash", str(SCRIPTS / "verify_v4_apk.sh"), str(apk), digest],
                check=True,
                env=environment,
            )

    def test_v3_apk_verifier_checks_legacy_identity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sdk = root / "sdk"
            build_tools = sdk / "build-tools/36.0.0"
            command_tools = sdk / "cmdline-tools/latest/bin"
            build_tools.mkdir(parents=True)
            command_tools.mkdir(parents=True)
            digest = "1" * 64
            apksigner = build_tools / "apksigner"
            apksigner.write_text(
                f"#!/bin/sh\necho 'Signer #1 certificate SHA-256 digest: {digest}'\n",
                encoding="utf-8",
            )
            apkanalyzer = command_tools / "apkanalyzer"
            apkanalyzer.write_text(
                "#!/bin/sh\n"
                "case \"$2\" in\n"
                "application-id) echo com.genymobile.gnirehtet ;;\n"
                "version-code) echo 10 ;;\n"
                "version-name) echo 3.0.0 ;;\n"
                "min-sdk) echo 21 ;;\n"
                "target-sdk) echo 29 ;;\n"
                "debuggable) echo false ;;\n"
                "*) exit 2 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            apksigner.chmod(0o755)
            apkanalyzer.chmod(0o755)
            apk = root / "standard.apk"
            apk.write_bytes(b"signed-v3-apk")
            environment = os.environ.copy()
            environment["ANDROID_HOME"] = str(sdk)
            environment["ANDROID_SDK_ROOT"] = str(sdk)
            subprocess.run(
                ["bash", str(SCRIPTS / "verify_v3_apk.sh"), str(apk), digest],
                check=True,
                env=environment,
            )


class ReleasePolicyTest(unittest.TestCase):
    def test_gradle_distributions_are_checksum_pinned(self) -> None:
        for properties in (
            REPOSITORY / "gradle/wrapper/gradle-wrapper.properties",
            REPOSITORY / "android-v4/gradle/wrapper/gradle-wrapper.properties",
        ):
            text = properties.read_text(encoding="utf-8")
            match = re.search(r"^distributionSha256Sum=([0-9a-f]{64})$", text, re.MULTILINE)
            self.assertIsNotNone(match, f"unverified Gradle distribution: {properties}")

    def test_github_actions_remain_disabled(self) -> None:
        workflows = sorted((REPOSITORY / ".github/workflows").glob("*.yml"))
        self.assertEqual(workflows, [])

    def test_comparator_is_locked_audited_and_not_released(self) -> None:
        comparator = (
            REPOSITORY / "benchmarks/comparators/tun2proxy/Cargo.toml"
        ).read_text(encoding="utf-8")
        lock = (REPOSITORY / "benchmarks/comparators/tun2proxy/Cargo.lock").read_text(
            encoding="utf-8"
        )
        notices = (
            REPOSITORY / "benchmarks/comparators/tun2proxy/THIRD_PARTY_NOTICES.md"
        ).read_text(encoding="utf-8")
        release = (REPOSITORY / "scripts/build_v4_windows_rc.ps1").read_text(
            encoding="utf-8"
        )
        revision = "eed123fbbec06295bf83f9be36d5a0f64ed9a8cb"
        self.assertIn(revision, comparator)
        self.assertIn(revision, lock)
        self.assertIn("GPL-3.0-or-later", notices)
        self.assertIn("WTFPL", notices)
        product_policy = (REPOSITORY / "host-rust/deny.toml").read_text(encoding="utf-8")
        self.assertNotIn("GPL-3.0-or-later", product_policy)
        self.assertNotIn("WTFPL", product_policy)
        self.assertNotIn("benchmarks/comparators", release)
        self.assertIn('"--locked"', release)
        self.assertIn("target-feature=+crt-static", release)

    def test_current_and_legacy_rollout_is_publishable_and_user_facing(self) -> None:
        readme = (REPOSITORY / "README.md").read_text(encoding="utf-8")
        android_v4 = (REPOSITORY / "android-v4/app/build.gradle.kts").read_text(
            encoding="utf-8"
        )
        android_strings = (
            REPOSITORY / "android-v4/app/src/main/res/values/strings.xml"
        ).read_text(encoding="utf-8")
        rust_v4 = (REPOSITORY / "host-rust/Cargo.toml").read_text(encoding="utf-8")
        ignore = (REPOSITORY / ".gitignore").read_text(encoding="utf-8")

        self.assertIn('<h1 align="center">Quest VD Wired</h1>', readme)
        self.assertIn("Download v4.1.7", readme)
        self.assertIn("quest-vd-wired-v4.1.7-windows-x64.zip", readme)
        self.assertIn("quest-vd-wired.exe", readme)
        self.assertNotIn("docs/", readme)
        self.assertIn("/docs/", ignore)
        self.assertTrue((REPOSITORY / "release").is_file())
        self.assertTrue((REPOSITORY / "scripts/build_v4_android_rc.sh").is_file())
        self.assertTrue((REPOSITORY / "scripts/build_v4_windows_rc.ps1").is_file())
        self.assertIn('versionCode = 56', android_v4)
        self.assertIn('versionName = "4.1.4"', android_v4)
        self.assertIn('<string name="app_name">Quest VD Wired</string>', android_strings)
        self.assertIn('version = "4.1.7"', rust_v4)

    def test_windows_executable_uses_the_green_tray_icon(self) -> None:
        crate = (REPOSITORY / "host-rust/crates/gnirehtet-vd/Cargo.toml").read_text(
            encoding="utf-8"
        )
        build = (REPOSITORY / "host-rust/crates/gnirehtet-vd/build.rs").read_text(
            encoding="utf-8"
        )
        resource = (
            REPOSITORY / "host-rust/crates/gnirehtet-vd/assets/windows.rc"
        ).read_text(encoding="utf-8")

        self.assertIn('embed-resource = "=3.0.7"', crate)
        self.assertIn('embed_resource::compile_for(', build)
        self.assertIn('"assets/windows.rc"', build)
        self.assertIn('["quest-vd-wired"]', build)
        self.assertIn('VERSION_MAJOR=', build)
        self.assertIn('1 ICON "tray-on.ico"', resource)
        self.assertIn("1 VERSIONINFO", resource)
        self.assertIn('VALUE "ProductName", "Quest VD Wired\\0"', resource)
        self.assertIn('VALUE "OriginalFilename", "quest-vd-wired.exe\\0"', resource)

        builder = (REPOSITORY / "scripts/build_v4_windows_rc.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("quest-vd-wired.exe", builder)
        self.assertNotIn("gnirehtet-vd.exe", builder)
        self.assertIn("ProductVersion", builder)
        self.assertIn('("ICON", "GROUP_ICON", "VERSION")', builder)

    def test_android_release_dependency_compliance_is_fail_closed(self) -> None:
        gradle = (REPOSITORY / "android-v4/app/build.gradle.kts").read_text(encoding="utf-8")
        builder = (REPOSITORY / "scripts/build_v4_android_rc.sh").read_text(encoding="utf-8")
        verifier = (REPOSITORY / "scripts/verify_v4_apk.sh").read_text(encoding="utf-8")
        notices = (
            REPOSITORY / "android-v4/app/src/main/assets/THIRD_PARTY_NOTICES.md"
        ).read_text(encoding="utf-8")

        for coordinate in (
            "org.jetbrains.kotlin:kotlin-stdlib:2.2.10",
            "org.jetbrains:annotations:13.0",
        ):
            self.assertIn(coordinate, gradle)
            self.assertIn(coordinate, verifier)
            self.assertIn(coordinate, notices)
        self.assertIn("outputs.upToDateWhen { false }", gradle)
        self.assertIn("testReleaseRuntimeClasspathGuard", builder)
        self.assertIn("verifyReleaseRuntimeClasspath", builder)
        self.assertIn("gnirehtet-v4-android.cdx.json", builder)
        self.assertIn("ANDROID_THIRD_PARTY_NOTICES.md", builder)
        self.assertNotIn("gnirehtet-v4-android-native.cdx.json", builder)
        self.assertNotIn("ANDROID_NATIVE_NOTICES.md", builder)

    def test_windows_release_remaps_and_rejects_local_build_paths(self) -> None:
        builder = (REPOSITORY / "scripts/build_v4_windows_rc.ps1").read_text(
            encoding="utf-8"
        )
        self.assertIn("CARGO_ENCODED_RUSTFLAGS", builder)
        self.assertIn("[char]0x1f", builder)
        self.assertIn("--remap-path-prefix", builder)
        self.assertIn("target-feature=+crt-static", builder)
        self.assertIn("link-arg=/Brepro", builder)
        self.assertIn("verify_windows_release.py", builder)
        self.assertIn("llvm-readobj", builder)
        self.assertIn('Get-Command "llvm-readobj" -ErrorAction Stop', builder)


if __name__ == "__main__":
    unittest.main()
