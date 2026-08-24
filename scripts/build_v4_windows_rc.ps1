param(
    [Parameter(Mandatory = $true)]
    [string]$ApkPath,
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory,
    [Parameter(Mandatory = $true)]
    [string]$AndroidArtifactsDirectory
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifest = Join-Path $repoRoot "host-rust\Cargo.toml"
$apk = (Resolve-Path $ApkPath).Path
$androidArtifacts = (Resolve-Path $AndroidArtifactsDirectory).Path
$output = [System.IO.Path]::GetFullPath($OutputDirectory)
$target = "x86_64-pc-windows-msvc"
$expectedVersion = "4.1.5"
$builtExe = Join-Path $repoRoot "host-rust\target\$target\release\quest-vd-wired.exe"

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE"
    }
}

foreach ($command in @("cargo", "llvm-readobj", "python")) {
    if (-not (Get-Command $command -ErrorAction SilentlyContinue)) {
        throw "required command is unavailable: $command"
    }
}
if (-not (Test-Path -LiteralPath $apk -PathType Leaf)) {
    throw "signed Android v4 APK is missing"
}

if (Test-Path -LiteralPath $output) {
    Remove-Item -LiteralPath $output -Recurse -Force
}
New-Item -ItemType Directory -Path $output | Out-Null

$env:GNIREHTET_VD_APK = $apk
$env:CARGO_INCREMENTAL = "0"
$rustFlags = [System.Collections.Generic.List[string]]::new()
foreach ($flag in @("-C", "target-feature=+crt-static", "-C", "link-arg=/Brepro")) {
    $rustFlags.Add($flag)
}
$remapCandidates = @(
    [PSCustomObject]@{ Source = $repoRoot; Target = "/source/quest-vd-wired" }
    [PSCustomObject]@{ Source = $env:USERPROFILE; Target = "/source/user" }
    [PSCustomObject]@{ Source = $env:HOME; Target = "/source/user" }
    [PSCustomObject]@{ Source = $env:CARGO_HOME; Target = "/source/cargo" }
    [PSCustomObject]@{ Source = $env:RUSTUP_HOME; Target = "/source/rustup" }
    [PSCustomObject]@{ Source = $env:TEMP; Target = "/source/temp" }
    [PSCustomObject]@{ Source = $env:TMP; Target = "/source/temp" }
)
$seenRemapRoots = @{}
foreach ($candidate in $remapCandidates) {
    $source = $candidate.Source
    if ([string]::IsNullOrWhiteSpace($source)) { continue }
    $source = [System.IO.Path]::GetFullPath($source).TrimEnd([char[]]@('\', '/'))
    if ($source.Length -lt 4) { continue }
    $key = $source.ToLowerInvariant()
    if ($seenRemapRoots.ContainsKey($key)) { continue }
    $seenRemapRoots[$key] = $true
    $rustFlags.Add("--remap-path-prefix")
    $rustFlags.Add("${source}=$($candidate.Target)")
}
$env:CARGO_ENCODED_RUSTFLAGS = [string]::Join([char]0x1f, $rustFlags)
Remove-Item Env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -ErrorAction SilentlyContinue
if (-not $env:SOURCE_DATE_EPOCH) {
    $env:SOURCE_DATE_EPOCH = (& git -C $repoRoot show -s --format=%ct HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw "could not determine SOURCE_DATE_EPOCH" }
}

$localRoots = @(
    $repoRoot,
    $env:USERPROFILE,
    $env:HOME,
    $env:CARGO_HOME,
    $env:RUSTUP_HOME,
    $env:TEMP,
    $env:TMP
) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Select-Object -Unique
$llvmReadObj = Get-Command "llvm-readobj" -ErrorAction Stop
function Test-ReleaseExecutable {
    param([string]$Executable)
    $arguments = @(
        (Join-Path $repoRoot "scripts\verify_windows_release.py"),
        $Executable
    )
    foreach ($localRoot in $localRoots) {
        $arguments += @("--local-root", $localRoot)
    }
    $arguments += @(
        "--llvm-readobj", $llvmReadObj.Source,
        "--product-name", "Quest VD Wired",
        "--original-filename", "quest-vd-wired.exe",
        "--product-version", $expectedVersion
    )
    Invoke-Checked "python" $arguments

    $versionInfo = (Get-Item -LiteralPath $Executable).VersionInfo
    $expectedValues = @{
        ProductName = "Quest VD Wired"
        FileDescription = "Quest VD Wired"
        OriginalFilename = "quest-vd-wired.exe"
        InternalName = "quest-vd-wired"
        FileVersion = "$expectedVersion.0"
        ProductVersion = $expectedVersion
    }
    foreach ($field in $expectedValues.Keys) {
        if ($versionInfo.$field -ne $expectedValues[$field]) {
            throw "Windows version field $field is '$($versionInfo.$field)', expected '$($expectedValues[$field])'"
        }
    }

    $resources = & $llvmReadObj.Source --coff-resources $Executable
    if ($LASTEXITCODE -ne 0) {
        throw "llvm-readobj could not inspect Windows resources"
    }
    $resourcesText = $resources -join "`n"
    foreach ($resourceType in @("ICON", "GROUP_ICON", "VERSION")) {
        if ($resourcesText -notmatch "Type:\s+$resourceType") {
            throw "Windows executable is missing the $resourceType resource"
        }
    }
}

Invoke-Checked "cargo" @("fmt", "--manifest-path", $manifest, "--all", "--", "--check")
Invoke-Checked "cargo" @("deny", "--manifest-path", $manifest, "--config", (Join-Path $repoRoot "host-rust\deny.toml"), "check")
Invoke-Checked "cargo" @("clippy", "--manifest-path", $manifest, "--locked", "--target", $target, "--all-targets", "--", "-D", "warnings")
Invoke-Checked "cargo" @("test", "--manifest-path", $manifest, "--locked", "--target", $target, "--all-targets")
Invoke-Checked "cargo" @("build", "--manifest-path", $manifest, "--locked", "--target", $target, "--release")
Invoke-Checked "python" @((Join-Path $repoRoot "scripts\normalize_windows_pe.py"), $builtExe)

if (-not (Test-Path -LiteralPath $builtExe -PathType Leaf)) {
    throw "Windows x64 host executable was not produced"
}
Test-ReleaseExecutable $builtExe
$firstExe = Join-Path $output "quest-vd-wired.exe"
Copy-Item -LiteralPath $builtExe -Destination $firstExe
Invoke-Checked "python" @((Join-Path $repoRoot "scripts\verify_embedded_apk.py"), $firstExe, $apk)

Invoke-Checked "cargo" @("clean", "--manifest-path", $manifest, "--target", $target)
Invoke-Checked "cargo" @("build", "--manifest-path", $manifest, "--locked", "--target", $target, "--release")
Invoke-Checked "python" @((Join-Path $repoRoot "scripts\normalize_windows_pe.py"), $builtExe)
Test-ReleaseExecutable $builtExe
$firstHash = (Get-FileHash -LiteralPath $firstExe -Algorithm SHA256).Hash.ToLowerInvariant()
$secondHash = (Get-FileHash -LiteralPath $builtExe -Algorithm SHA256).Hash.ToLowerInvariant()
@(
    "artifact=quest-vd-wired.exe"
    "first_sha256=$firstHash"
    "second_sha256=$secondHash"
    "reproducible=$($firstHash -eq $secondHash)".ToLowerInvariant()
) | Set-Content -LiteralPath (Join-Path $output "WINDOWS_REPRODUCIBILITY.txt") -Encoding utf8
if ($firstHash -ne $secondHash) {
    throw "Windows x64 host is not reproducible across clean builds"
}
Invoke-Checked "python" @((Join-Path $repoRoot "scripts\verify_embedded_apk.py"), $builtExe, $apk)

$rawSbom = Join-Path $repoRoot "host-rust\crates\gnirehtet-vd\gnirehtet-vd.cdx.json"
if (Test-Path -LiteralPath $rawSbom) {
    Remove-Item -LiteralPath $rawSbom -Force
}
Invoke-Checked "cargo" @(
    "cyclonedx",
    "--manifest-path", $manifest,
    "--format", "json",
    "--all",
    "--target", $target,
    "--spec-version", "1.5",
    "--override-filename", "gnirehtet-vd.cdx"
)
if (-not (Test-Path -LiteralPath $rawSbom -PathType Leaf)) {
    throw "Rust CycloneDX SBOM was not produced at the expected path"
}
Invoke-Checked "python" @(
    (Join-Path $repoRoot "scripts\sanitize_rust_sbom.py"),
    $rawSbom,
    (Join-Path $output "gnirehtet-vd-rust.cdx.json"),
    "--manifest", $manifest,
    "--repository-root", $repoRoot
)
Remove-Item -LiteralPath $rawSbom -Force
Invoke-Checked "python" @(
    (Join-Path $repoRoot "scripts\generate_rust_notices.py"),
    "--manifest", $manifest,
    "--output", (Join-Path $output "RUST_THIRD_PARTY_NOTICES.md")
)
Copy-Item -LiteralPath $apk -Destination (Join-Path $output "gnirehtet-v4.apk")
Copy-Item -LiteralPath (Join-Path $repoRoot "LICENSE") -Destination (Join-Path $output "PROJECT_LICENSE.txt")
$androidEvidence = Join-Path $output "android"
New-Item -ItemType Directory -Path $androidEvidence | Out-Null
Copy-Item -Path (Join-Path $androidArtifacts "*") -Destination $androidEvidence -Recurse
Invoke-Checked "python" @(
    (Join-Path $repoRoot "scripts\write_sha256.py"),
    $output,
    "--output", (Join-Path $output "SHA256SUMS")
)
