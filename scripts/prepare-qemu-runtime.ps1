param(
    [Parameter(Mandatory = $true)][string]$RuntimeVersion,
    [Parameter(Mandatory = $true)][string]$GuestAssetDir,
    [ValidateSet("windows-x86_64", "windows-aarch64")][string]$TargetPlatform = "windows-x86_64",
    [string]$Msys2Root
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

$qemuExe = $null
$qemuRoot = $null
$sourceModules = $null
$sourceShare = $null
if ($TargetPlatform -eq "windows-aarch64") {
    if ([string]::IsNullOrWhiteSpace($Msys2Root)) {
        throw "Msys2Root is required for the native Windows ARM64 QEMU runtime"
    }
    $qemuRoot = Join-Path $Msys2Root "clangarm64"
    $qemuExe = Get-Item -LiteralPath (Join-Path $qemuRoot "bin/qemu-system-aarch64.exe") -ErrorAction SilentlyContinue
    if (-not $qemuExe) {
        throw "Native ARM64 qemu-system-aarch64.exe was not found under $qemuRoot"
    }
    $ntlddExe = Join-Path $qemuRoot "bin/ntldd.exe"
    if (-not (Test-Path -LiteralPath $ntlddExe -PathType Leaf)) {
        throw "ntldd.exe was not found under $qemuRoot; install mingw-w64-clang-aarch64-ntldd"
    }
    $sourceShare = Join-Path $qemuRoot "share/qemu"
}
else {
    $qemuSearchRoots = @(
        (Join-Path $env:ProgramFiles "qemu"),
        (Join-Path $env:ProgramData "chocolatey/lib/qemu/tools"),
        (Join-Path $env:ProgramFiles "")
    )
    if (${env:ProgramFiles(x86)}) {
        $qemuSearchRoots += Join-Path ${env:ProgramFiles(x86)} "qemu"
    }
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
}

if (Test-Path -LiteralPath $runtimeRoot) {
    Remove-Item -LiteralPath $runtimeRoot -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $binDir, $libDir, (Join-Path $runtimeRoot "boot"), $licenseDir | Out-Null
if ($TargetPlatform -eq "windows-aarch64") {
    Copy-Item -LiteralPath $qemuExe.FullName -Destination $binDir -Force

    # MSYS2's QEMU is a native ARM64 build. Walk its PE imports recursively,
    # copying only MSYS2 runtime DLLs. Let Windows resolve OS libraries itself;
    # asking ntldd -R to recurse through the Windows DLL tree is both noisy and
    # slow on ARM64 runners.
    $optionalWindowsImportPattern = '(?i)^(api-ms-|ext-ms-|AzureAttest.*\.dll$|HvsiFileTrust\.dll$|PdmUtilities\.dll$|wpaxholder\.dll$)'
    $systemDllPattern = '(?i)\\Windows\\(System32|WinSxS)\\'
    $env:PATH = "$($qemuRoot)\bin;$Msys2Root\usr\bin;$env:PATH"
    $pendingRuntimeBinaries = [System.Collections.Generic.Queue[string]]::new()
    $pendingRuntimeBinaries.Enqueue($qemuExe.FullName)
    if ($sourceModules -and (Test-Path -LiteralPath $sourceModules -PathType Container)) {
        # QEMU loads these DLLs at runtime, so their imports are not reachable
        # from the main executable's PE import table alone.
        Get-ChildItem -LiteralPath $sourceModules -Filter "*.dll" -File -Recurse |
            ForEach-Object { $pendingRuntimeBinaries.Enqueue($_.FullName) }
    }
    $visitedRuntimeBinaries = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    $copiedDlls = @{}
    while ($pendingRuntimeBinaries.Count -gt 0) {
        $currentBinary = $pendingRuntimeBinaries.Dequeue()
        $currentBinaryPath = [IO.Path]::GetFullPath($currentBinary)
        if (-not $visitedRuntimeBinaries.Add($currentBinaryPath)) {
            continue
        }
        $dependencyOutput = @(& $ntlddExe $currentBinaryPath 2>&1)
        if ($LASTEXITCODE -ne 0) {
            throw "Could not inspect QEMU DLL dependencies for $currentBinaryPath`: $($dependencyOutput -join ' ')"
        }
        foreach ($line in $dependencyOutput) {
            if ($line -match '(?i)^\s*(?<module>[^\s]+\.dll)\s*=>\s*not found\b') {
                $moduleName = [IO.Path]::GetFileName($Matches.module)
                if ($moduleName -notmatch $optionalWindowsImportPattern) {
                    throw "Could not resolve native QEMU runtime DLL $moduleName imported by $currentBinaryPath"
                }
                continue
            }
            if ($line -notmatch "(?i)^\s*(?<module>[^\s]+\.dll)(?:\s*=>\s*(?<resolved>.+?))?(?:\s+\(0x[0-9a-f]+\))?\s*$") {
                continue
            }
            $moduleName = [IO.Path]::GetFileName($Matches.module)
            $resolvedPath = $Matches.resolved
            if ($moduleName -notmatch '(?i)\.dll$' -or $moduleName -match $optionalWindowsImportPattern -or
                ($resolvedPath -and $resolvedPath -match $systemDllPattern)) {
                continue
            }
            $dependency = $null
            if ($resolvedPath -and (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
                $dependency = Get-Item -LiteralPath $resolvedPath
            }
            else {
                $candidatePaths = @(
                    (Join-Path (Split-Path -Parent $currentBinaryPath) $moduleName),
                    (Join-Path $qemuExe.Directory.FullName $moduleName)
                )
                foreach ($candidate in $candidatePaths) {
                    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                        $dependency = Get-Item -LiteralPath $candidate
                        break
                    }
                }
            }
            if (-not $dependency) {
                throw "QEMU dependency $moduleName was not resolved from the MSYS2 runtime or Windows"
            }
            if (-not $copiedDlls.ContainsKey($dependency.Name)) {
                Copy-Item -LiteralPath $dependency.FullName -Destination (Join-Path $binDir $dependency.Name) -Force
                $copiedDlls[$dependency.Name] = $true
                $pendingRuntimeBinaries.Enqueue($dependency.FullName)
            }
        }
    }
    Write-Output "copied $($copiedDlls.Count) native ARM64 QEMU DLL dependencies from $($visitedRuntimeBinaries.Count) PE files"
}
else {
    Get-ChildItem -Path $qemuExe.Directory.FullName -File | Copy-Item -Destination $binDir -Force
}
if ($sourceShare -and $TargetPlatform -eq "windows-aarch64") {
    $virtioRom = Join-Path $sourceShare "efi-virtio.rom"
    if (-not (Test-Path -LiteralPath $virtioRom -PathType Leaf)) {
        throw "Required QEMU ROM is missing: $virtioRom"
    }
    New-Item -ItemType Directory -Force -Path $shareDir | Out-Null
    Copy-Item -LiteralPath $virtioRom -Destination $shareDir -Force
    $msysLicenseRoot = Join-Path $qemuRoot "share/licenses"
    if (Test-Path -LiteralPath $msysLicenseRoot -PathType Container) {
        $msysLicenseDir = Join-Path $licenseDir "msys2"
        New-Item -ItemType Directory -Force -Path $msysLicenseDir | Out-Null
        Copy-Item -Path (Join-Path $msysLicenseRoot "*") -Destination $msysLicenseDir -Recurse -Force
    }
}
elseif ($sourceShare) {
    New-Item -ItemType Directory -Force -Path $shareDir | Out-Null
    Copy-Item -Path (Join-Path $sourceShare "*") -Destination $shareDir -Recurse -Force
}
if ($sourceModules -and (Test-Path -LiteralPath $sourceModules -PathType Container)) {
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

if ($TargetPlatform -eq "windows-x86_64") {
    $licenseFiles = Get-ChildItem -Path $qemuRoot -File -Recurse |
        Where-Object { $_.Name -like "COPYING*" -or $_.Name -like "LICENSE*" -or $_.Name -like "COPYRIGHT*" -or $_.Name -like "NOTICE*" } |
        Select-Object -First 20
    foreach ($license in $licenseFiles) {
        Copy-Item -LiteralPath $license.FullName -Destination (Join-Path $licenseDir $license.Name) -Force
    }
}

$targetQemu = Join-Path $binDir "qemu-system-aarch64.exe"
$stream = [IO.File]::OpenRead($targetQemu)
$reader = $null
try {
    $reader = [IO.BinaryReader]::new($stream)
    if ($reader.ReadUInt16() -ne 0x5A4D) { throw "QEMU is not a Windows PE executable: $targetQemu" }
    $stream.Position = 0x3C
    $peOffset = $reader.ReadInt32()
    if ($peOffset -lt 0 -or $peOffset + 6 -gt $stream.Length) { throw "QEMU has an invalid PE header: $targetQemu" }
    $stream.Position = $peOffset
    if ($reader.ReadUInt32() -ne 0x00004550) { throw "QEMU has an invalid PE signature: $targetQemu" }
    $machine = $reader.ReadUInt16()
}
finally {
    if ($reader) { $reader.Dispose() } else { $stream.Dispose() }
}
$expectedMachine = if ($TargetPlatform -eq "windows-aarch64") { 0xAA64 } else { 0x8664 }
if ($machine -ne $expectedMachine) {
    throw ("Expected QEMU PE machine 0x{0:X4}, found 0x{1:X4}" -f $expectedMachine, $machine)
}
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
if ($TargetPlatform -eq "windows-aarch64") {
    $qemuSource = "MSYS2 CLANGARM64 QEMU package"
    $qemuSourceUrl = "https://packages.msys2.org/package/mingw-w64-clang-aarch64-qemu"
    $qemuSourceCodeUrl = "https://github.com/msys2/MINGW-packages/tree/master/mingw-w64-qemu"
}
else {
    $qemuSource = "Chocolatey QEMU package"
    $qemuSourceUrl = "https://community.chocolatey.org/packages/qemu"
    $qemuSourceCodeUrl = "https://download.qemu.org/"
}
@{
    runtime_version = $RuntimeVersion
    target_platform = $TargetPlatform
    guest_os = "Alpine Linux"
    guest_version = "3.23.8"
    guest_source_url = "https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-3.23.8/"
    qemu_version = $qemuVersion
    qemu_source = $qemuSource
    qemu_source_url = $qemuSourceUrl
    qemu_source_code_url = $qemuSourceCodeUrl
    qemu_sha256 = $manifestAssets[2].sha256
    support_assets = $supportAssets
    license_files = @(Get-ChildItem -Path $licenseDir -File -Recurse | ForEach-Object {
        $_.FullName.Substring($runtimeRoot.Length + 1).Replace("\", "/")
    })
} | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 (Join-Path $runtimeRoot "runtime-info.json")

$archive = Join-Path $env:RUNNER_TEMP "rootglass-runtime-$TargetPlatform-$RuntimeVersion.zip"
Compress-Archive -Path $runtimeRoot -DestinationPath $archive -CompressionLevel Optimal -Force
$digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
"$digest  $([IO.Path]::GetFileName($archive))" | Set-Content -Encoding ascii "$archive.sha256"
Write-Output "runtime archive: $archive"
