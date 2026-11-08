param(
    [Parameter(Mandatory = $true)][string]$RuntimeVersion,
    [Parameter(Mandatory = $true)][string]$GuestAssetDir
)

$ErrorActionPreference = "Stop"
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$runtimeRoot = Join-Path $repoRoot "crates/rootglass-desktop/runtime-assets/$RuntimeVersion"
$binDir = Join-Path $runtimeRoot "bin"
$libDir = Join-Path $runtimeRoot "lib"
$shareDir = Join-Path $runtimeRoot "share/qemu"
$licenseDir = Join-Path $runtimeRoot "licenses"

foreach ($asset in @("vmlinuz-virt", "initramfs-virt")) {
    $source = Join-Path $GuestAssetDir $asset
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Guest runtime asset is missing: $source"
    }
}

$qemuSearchRoots = @(
    (Join-Path $env:ProgramFiles "qemu"),
    (Join-Path $env:ProgramData "chocolatey/lib/qemu/tools"),
    (Join-Path $env:ProgramFiles "")
)
if (${env:ProgramFiles(x86)}) {
    $qemuSearchRoots += Join-Path ${env:ProgramFiles(x86)} "qemu"
}
$qemuExe = $null
foreach ($searchRoot in $qemuSearchRoots) {
    if (Test-Path -LiteralPath $searchRoot -PathType Container) {
        $qemuExe = Get-ChildItem -Path $searchRoot -Filter "qemu-system-aarch64.exe" -File -Recurse -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($qemuExe) { break }
    }
}
if (-not $qemuExe) {
    throw "qemu-system-aarch64.exe was not found in the Chocolatey or Program Files locations"
}
$qemuRoot = $qemuExe.Directory
if ($qemuRoot.Name -eq "bin") { $qemuRoot = $qemuRoot.Parent }
$sourceShare = Join-Path $qemuRoot "share/qemu"
if (-not (Test-Path -LiteralPath $sourceShare -PathType Container)) {
    # Some Windows QEMU builds install their data files directly in `share`;
    # others omit external share data because the packaged direct-kernel boot
    # path does not need firmware images.
    $flatShare = Join-Path $qemuRoot "share"
    if (Test-Path -LiteralPath $flatShare -PathType Container) {
        $sourceShare = $flatShare
    }
    else {
        $sourceShare = $null
        Write-Warning "QEMU has no external share data; the runtime will use QEMU's built-in data search path"
    }
}
$sourceModules = Join-Path $qemuRoot "lib/qemu"

if (Test-Path -LiteralPath $runtimeRoot) {
    Remove-Item -LiteralPath $runtimeRoot -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $binDir, $libDir, (Join-Path $runtimeRoot "boot"), $licenseDir | Out-Null
Get-ChildItem -Path $qemuExe.Directory.FullName -File | Copy-Item -Destination $binDir -Force
if ($sourceShare) {
    New-Item -ItemType Directory -Force -Path $shareDir | Out-Null
    Copy-Item -Path (Join-Path $sourceShare "*") -Destination $shareDir -Recurse -Force
}
if (Test-Path -LiteralPath $sourceModules -PathType Container) {
    New-Item -ItemType Directory -Force -Path (Join-Path $libDir "qemu") | Out-Null
    Copy-Item -Path (Join-Path $sourceModules "*") -Destination (Join-Path $libDir "qemu") -Recurse -Force
}
Copy-Item -LiteralPath (Join-Path $repoRoot "licenses/GPL-2.0.txt") -Destination (Join-Path $licenseDir "QEMU-GPL-2.0.txt")
@"
The kernel and initramfs in this runtime bundle are derived from the Alpine Linux
3.23.8 aarch64 netboot release. Alpine Linux and its included components are
distributed under their respective upstream licenses. Source and release files:
https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-3.23.8/
"@ | Set-Content -Encoding utf8 (Join-Path $licenseDir "ALPINE-RUNTIME-NOTICE.txt")
Copy-Item -LiteralPath (Join-Path $GuestAssetDir "vmlinuz-virt") -Destination (Join-Path $runtimeRoot "boot/vmlinuz-virt")
Copy-Item -LiteralPath (Join-Path $GuestAssetDir "initramfs-virt") -Destination (Join-Path $runtimeRoot "boot/initramfs-virt")

$licenseFiles = Get-ChildItem -Path $qemuRoot -File -Recurse |
    Where-Object { $_.Name -like "COPYING*" -or $_.Name -like "LICENSE*" -or $_.Name -like "COPYRIGHT*" -or $_.Name -like "NOTICE*" } |
    Select-Object -First 20
foreach ($license in $licenseFiles) {
    Copy-Item -LiteralPath $license.FullName -Destination (Join-Path $licenseDir $license.Name) -Force
}

$targetQemu = Join-Path $binDir "qemu-system-aarch64.exe"
$versionOutput = & $targetQemu --version
if ($LASTEXITCODE -ne 0) { throw "Could not run bundled QEMU to identify its version" }
$qemuVersion = ($versionOutput | Select-Object -First 1).Trim()
$manifestAssets = @(
    @{ kind = "kernel"; path = "boot/vmlinuz-virt" },
    @{ kind = "initramfs"; path = "boot/initramfs-virt" },
    @{ kind = "qemu"; path = "bin/qemu-system-aarch64.exe" }
)
foreach ($asset in $manifestAssets) {
    $asset.sha256 = (Get-FileHash -LiteralPath (Join-Path $runtimeRoot $asset.path) -Algorithm SHA256).Hash.ToLowerInvariant()
}
$supportPaths = @($binDir, $libDir)
if (Test-Path -LiteralPath $shareDir -PathType Container) {
    $supportPaths += $shareDir
}
$supportAssets = Get-ChildItem -Path $supportPaths -File -Recurse |
    Where-Object { $_.FullName -ne $targetQemu } |
    Sort-Object FullName |
    ForEach-Object {
        $relativePath = $_.FullName.Substring($runtimeRoot.Length + 1).Replace("\", "/")
        @{
            kind = "support"
            path = $relativePath
            sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
$allAssets = @($manifestAssets) + @($supportAssets)
@{
    schema_version = 1
    runtime_version = $RuntimeVersion
    assets = $allAssets
} | ConvertTo-Json -Depth 5 | Set-Content -Encoding utf8 (Join-Path $runtimeRoot "assets.json")
@{
    runtime_version = $RuntimeVersion
    target_platform = "windows-x86_64"
    guest_os = "Alpine Linux"
    guest_version = "3.23.8"
    guest_source_url = "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-3.23.8/"
    qemu_version = $qemuVersion
    qemu_source = "Chocolatey QEMU package"
    qemu_source_url = "https://community.chocolatey.org/packages/qemu"
    qemu_source_code_url = "https://download.qemu.org/"
    qemu_sha256 = $manifestAssets[2].sha256
    support_assets = $supportAssets
    license_files = @(Get-ChildItem -Path $licenseDir -File -Recurse | ForEach-Object {
        $_.FullName.Substring($runtimeRoot.Length + 1).Replace("\", "/")
    })
} | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $runtimeRoot "runtime-info.json")

$archive = Join-Path $env:RUNNER_TEMP "rootglass-runtime-windows-x86_64-$RuntimeVersion.zip"
Compress-Archive -Path $runtimeRoot -DestinationPath $archive -CompressionLevel Optimal -Force
$digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$digest  $([IO.Path]::GetFileName($archive))" | Set-Content -Encoding ascii "$archive.sha256"
Write-Output "runtime archive: $archive"
