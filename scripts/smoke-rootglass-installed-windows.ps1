param(
    [Parameter(Mandatory = $true)][string]$PackageDirectory,
    [Parameter(Mandatory = $true)][string]$ExampleDirectory,
    [Parameter(Mandatory = $true)][string]$EvidenceDirectory,
    [Parameter(Mandatory = $true)][string]$TuiSmokeScript
)

$ErrorActionPreference = "Stop"
$PackageDirectory = (Resolve-Path -LiteralPath $PackageDirectory).Path
$ExampleDirectory = (Resolve-Path -LiteralPath $ExampleDirectory).Path
$EvidenceDirectory = [IO.Path]::GetFullPath($EvidenceDirectory)
New-Item -ItemType Directory -Force -Path $EvidenceDirectory | Out-Null
$ProgressFile = Join-Path $EvidenceDirectory "progress.txt"
$FirewallNames = @()
$RootglassProcess = $null

function Write-ProgressLine([string]$Message) {
    $Message | Tee-Object -FilePath $ProgressFile -Append
}

function Verify-Checksums([string]$Root, [string]$ChecksumName) {
    $checksumFile = @(Get-ChildItem -LiteralPath $Root -Filter $ChecksumName -File -Recurse)
    if ($checksumFile.Count -ne 1) {
        throw "Expected one $ChecksumName under $Root; found $($checksumFile.Count)"
    }
    foreach ($line in Get-Content -LiteralPath $checksumFile[0].FullName) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if ($line -notmatch '^(?<hash>[0-9a-fA-F]{64})\s+\*?(?<name>.+)$') {
            throw "Invalid checksum row in $($checksumFile[0].FullName): $line"
        }
        $name = [IO.Path]::GetFileName($Matches.name.Trim())
        $matchingFiles = @(Get-ChildItem -LiteralPath $Root -Filter $name -File -Recurse)
        if ($matchingFiles.Count -ne 1) {
            throw "Expected one artifact named $name under $Root; found $($matchingFiles.Count)"
        }
        $actual = (Get-FileHash -LiteralPath $matchingFiles[0].FullName -Algorithm SHA256).Hash
        if ($actual -ine $Matches.hash) {
            throw "SHA-256 mismatch for $($matchingFiles[0].FullName)"
        }
        Write-ProgressLine "verified $name SHA-256 $($actual.ToLowerInvariant())"
    }
}

function Get-RootglassQemuProcesses([string]$QemuPath) {
    $env:ROOTGLASS_QEMU_PATH = $QemuPath
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='qemu-system-aarch64.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $env:ROOTGLASS_QEMU_PATH })
    Remove-Item Env:\ROOTGLASS_QEMU_PATH -ErrorAction SilentlyContinue
    return $processes
}

try {
    $progress = Join-Path $EvidenceDirectory "progress.txt"
    "Windows installed package smoke" | Set-Content -LiteralPath $progress -Encoding utf8
    Write-ProgressLine "verifying package and signed example checksums"
    Verify-Checksums $PackageDirectory "SHA256SUMS-windows.txt"
    Verify-Checksums $ExampleDirectory "SHA256SUMS-example.txt"

    $installer = @(Get-ChildItem -LiteralPath $PackageDirectory -Filter "*-setup.exe" -File -Recurse)
    $glass = @(Get-ChildItem -LiteralPath $ExampleDirectory -Filter "*.glass" -File -Recurse)
    if ($installer.Count -ne 1) { throw "Expected one Windows NSIS installer; found $($installer.Count)" }
    if ($glass.Count -ne 1) { throw "Expected one signed .glass example; found $($glass.Count)" }
    $installerHash = (Get-FileHash -LiteralPath $installer[0].FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    $glassHash = (Get-FileHash -LiteralPath $glass[0].FullName -Algorithm SHA256).Hash.ToLowerInvariant()

    $existingRootglass = @(Get-ChildItem -LiteralPath $env:ProgramFiles, "$env:LOCALAPPDATA\Programs" `
        -Filter "rootglass.exe" -File -Recurse -ErrorAction SilentlyContinue)
    if ($existingRootglass.Count -gt 0) {
        throw "The runner already has Rootglass installed; refusing to reuse existing state."
    }
    Write-ProgressLine "installing $($installer[0].Name) silently"
    $install = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S" -Wait -PassThru
    if ($install.ExitCode -ne 0) { throw "NSIS installer exited with $($install.ExitCode)" }

    $rootglassCandidates = @()
    foreach ($root in @($env:ProgramFiles, "$env:LOCALAPPDATA\Programs", $env:LOCALAPPDATA)) {
        if (Test-Path -LiteralPath $root -PathType Container) {
            $rootglassCandidates += @(Get-ChildItem -LiteralPath $root -Filter "rootglass.exe" `
                -File -Recurse -ErrorAction SilentlyContinue)
        }
    }
    $rootglassCandidates = @($rootglassCandidates | Where-Object {
        Test-Path -LiteralPath (Join-Path $_.DirectoryName "rootglass\runtime-assets")
    } | Sort-Object FullName -Unique)
    if ($rootglassCandidates.Count -ne 1) {
        throw "Expected one installed rootglass.exe with bundled runtime assets; found $($rootglassCandidates.Count)"
    }
    $RootglassExe = $rootglassCandidates[0].FullName
    $installRoot = $rootglassCandidates[0].DirectoryName
    Write-ProgressLine "installed executable: $RootglassExe"
    & $RootglassExe --help *> (Join-Path $EvidenceDirectory "installed-help.txt")
    if ($LASTEXITCODE -ne 0) { throw "Installed Rootglass --help exited with $LASTEXITCODE" }

    $runtimeRoot = Join-Path $installRoot "rootglass\runtime-assets"
    $runtimeManifests = @(Get-ChildItem -LiteralPath $runtimeRoot -Filter "assets.json" -File -Recurse)
    if ($runtimeManifests.Count -ne 1) {
        throw "Expected one installed runtime assets.json; found $($runtimeManifests.Count)"
    }
    $versionRoot = $runtimeManifests[0].DirectoryName
    $manifest = Get-Content -LiteralPath $runtimeManifests[0].FullName -Raw | ConvertFrom-Json
    $runtimeInfo = Get-Content -LiteralPath (Join-Path $versionRoot "runtime-info.json") -Raw | ConvertFrom-Json
    if ($runtimeInfo.target_platform -ne "windows-x86_64") {
        throw "Expected windows-x86_64 runtime, found $($runtimeInfo.target_platform)"
    }
    $inventory = @(
        "runtime version: $($manifest.runtime_version)",
        "target platform: $($runtimeInfo.target_platform)",
        "QEMU: $($runtimeInfo.qemu_version)"
    )
    foreach ($asset in $manifest.assets) {
        $assetPath = Join-Path $versionRoot ($asset.path -replace '/', '\')
        if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) {
            throw "Installed runtime asset is missing: $assetPath"
        }
        $actual = (Get-FileHash -LiteralPath $assetPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $asset.sha256.ToLowerInvariant()) {
            throw "Installed runtime asset checksum mismatch: $assetPath"
        }
        $inventory += "$($asset.kind) $($asset.path) $actual"
    }
    $inventory | Set-Content -LiteralPath (Join-Path $EvidenceDirectory "runtime-inventory.txt") -Encoding utf8
    $qemuExe = Join-Path $versionRoot "bin\qemu-system-aarch64.exe"
    $qemuVersion = (& $qemuExe --version | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0) { throw "Bundled QEMU could not report its version" }
    Write-ProgressLine "bundled runtime inventory and QEMU executable verified"

    $trustDirectory = Join-Path $env:APPDATA "Rootglass"
    $trustPath = Join-Path $trustDirectory "trust.json"
    $libraryRoot = Join-Path $env:LOCALAPPDATA "Rootglass\library"
    $stateRoot = Join-Path $env:LOCALAPPDATA "Rootglass\states"
    $existingDataPaths = @()
    foreach ($candidate in @($trustPath, $libraryRoot, $stateRoot)) {
        if (Test-Path -LiteralPath $candidate) {
            $existingDataPaths += $candidate
        }
    }
    if ($existingDataPaths.Count -gt 0) {
        throw "Rootglass user data existed before this smoke; refusing to overwrite it: $($existingDataPaths -join ', ')"
    }
    New-Item -ItemType Directory -Force -Path $trustDirectory | Out-Null
    @{
        schemaVersion = 1
        trustedPublishers = @(@{
            name = "Rootglass Examples"
            identity = "rootglass.dev/examples"
            keyId = "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0"
            publicKeyBase64url = "2To2Sg0jWnaNlkRA4GCUa3qbqmZkaw5IMCHh0vRnRpw"
        })
        revokedKeyIds = @()
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $trustPath -Encoding utf8
    Write-ProgressLine "configured the example publisher key in the disposable runner profile"

    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class RootglassSmokeWindow {
    [StructLayout(LayoutKind.Sequential)]
    private struct Rect {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
    [StructLayout(LayoutKind.Sequential)]
    private struct Point {
        public int X;
        public int Y;
    }
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern IntPtr SetActiveWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    private static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")]
    private static extern bool ClientToScreen(IntPtr window, ref Point point);
    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    private static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);

    public static string ClickImportGlass(IntPtr window) {
        Rect client;
        if (!GetClientRect(window, out client)) {
            throw new InvalidOperationException("Could not read the Rootglass client area.");
        }
        // The desktop smoke runner opens Rootglass maximized. Import Glass is
        // in the upper-right of the library view, at 94% width and 41% height.
        Point point = new Point {
            X = client.Right - 64,
            Y = (int)(client.Bottom * 0.406)
        };
        if (!ClientToScreen(window, ref point) || !SetCursorPos(point.X, point.Y)) {
            throw new InvalidOperationException("Could not position the pointer over Import Glass.");
        }
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
        return point.X + "," + point.Y;
    }
}
"@
    $pickerLog = Join-Path $EvidenceDirectory "picker-events.log"
    $env:ROOTGLASS_DESKTOP_SMOKE = "1"
    $env:ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG = $pickerLog
    $env:ROOTGLASS_DESKTOP_SMOKE_EXPECTED_TRUST_KEY_ID = "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0"
    Write-ProgressLine "opening the example through the GPUI file picker"
    $RootglassProcess = Start-Process -FilePath $RootglassExe -PassThru
    $windowDeadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $windowDeadline) {
        $RootglassProcess.Refresh()
        if ($RootglassProcess.MainWindowHandle -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($RootglassProcess.MainWindowHandle -eq [IntPtr]::Zero) {
        throw "Installed Rootglass did not expose a foreground window for the picker smoke."
    }
    $foregroundSet = [RootglassSmokeWindow]::SetForegroundWindow($RootglassProcess.MainWindowHandle)
    [void][RootglassSmokeWindow]::SetActiveWindow($RootglassProcess.MainWindowHandle)
    Start-Sleep -Milliseconds 500
    $foregroundWindow = [RootglassSmokeWindow]::GetForegroundWindow()
    Write-ProgressLine "foreground request=$foregroundSet target=$($RootglassProcess.MainWindowHandle) active=$foregroundWindow"
    try {
        Add-Type -AssemblyName System.Drawing
        $screenBounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($screenBounds.Width, $screenBounds.Height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($screenBounds.Location, [System.Drawing.Point]::Empty, $screenBounds.Size)
        $bitmap.Save((Join-Path $EvidenceDirectory "desktop-before-picker.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $graphics.Dispose()
        $bitmap.Dispose()
    } catch {
        Write-ProgressLine "desktop screenshot capture failed: $($_.Exception.Message)"
    }
    $clickPoint = [RootglassSmokeWindow]::ClickImportGlass($RootglassProcess.MainWindowHandle)
    Write-ProgressLine "clicked Import Glass at screen coordinates $clickPoint"
    $pickerDeadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $pickerDeadline) {
        $pickerEvents = if (Test-Path -LiteralPath $pickerLog) { Get-Content -LiteralPath $pickerLog } else { @() }
        if ($pickerEvents -contains "ROOTGLASS_DESKTOP_PICKER_OPEN") { break }
        Start-Sleep -Milliseconds 250
    }
    if (-not (Test-Path -LiteralPath $pickerLog) -or
        (Get-Content -LiteralPath $pickerLog) -notcontains "ROOTGLASS_DESKTOP_PICKER_OPEN") {
        throw "Clicking Import Glass did not open the Rootglass file picker. Foreground request=$foregroundSet; active=$foregroundWindow."
    }
    [System.Windows.Forms.SendKeys]::SendWait("%n")
    [System.Windows.Forms.SendKeys]::SendWait($glass[0].FullName)
    [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
    $pickerMarkers = @(
        "ROOTGLASS_DESKTOP_OPEN_PATH_ACCEPTED",
        "ROOTGLASS_DESKTOP_OPEN_PATH_ARCHIVE",
        "ROOTGLASS_DESKTOP_STATE_READY"
    )
    $pickerDeadline = (Get-Date).AddMinutes(2)
    while ((Get-Date) -lt $pickerDeadline) {
        $pickerEvents = Get-Content -LiteralPath $pickerLog
        if (@($pickerMarkers | Where-Object { $pickerEvents -notcontains $_ }).Count -eq 0) { break }
        Start-Sleep -Milliseconds 250
    }
    $pickerEvents = Get-Content -LiteralPath $pickerLog
    foreach ($marker in $pickerMarkers) {
        if ($pickerEvents -notcontains $marker) {
            throw "The Rootglass picker did not complete '$marker'. Event log: $pickerLog"
        }
    }
    if (-not $RootglassProcess.HasExited) {
        if (-not $RootglassProcess.CloseMainWindow()) {
            Stop-Process -Id $RootglassProcess.Id -Force
        } elseif (-not $RootglassProcess.WaitForExit(10000)) {
            Stop-Process -Id $RootglassProcess.Id -Force
        }
    }
    $RootglassProcess = $null
    Write-ProgressLine "GPUI picker opened and imported the signed example"

    $extensionKey = Get-Item -LiteralPath "Registry::HKEY_CLASSES_ROOT\.glass" -ErrorAction Stop
    $programId = $extensionKey.GetValue("")
    if ([string]::IsNullOrWhiteSpace($programId)) {
        throw "The installed package did not register a .glass file association."
    }
    $openCommandKey = "Registry::HKEY_CLASSES_ROOT\$programId\shell\open\command"
    $openCommand = (Get-Item -LiteralPath $openCommandKey -ErrorAction Stop).GetValue("")
    if ($openCommand -notlike "*$RootglassExe*") {
        throw "The .glass association points to '$openCommand', not '$RootglassExe'."
    }
    $associationLog = Join-Path $EvidenceDirectory "association-events.log"
    $env:ROOTGLASS_DESKTOP_SMOKE = "1"
    $env:ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG = $associationLog
    $env:ROOTGLASS_DESKTOP_SMOKE_EXPECTED_TRUST_KEY_ID = "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0"
    Write-ProgressLine "opening the signed example through the registered Windows association"
    $RootglassProcess = Start-Process -FilePath $glass[0].FullName -PassThru
    $requiredMarkers = @(
        "ROOTGLASS_DESKTOP_OPEN_PATH_ACCEPTED",
        "ROOTGLASS_DESKTOP_OPEN_PATH_ARCHIVE",
        "ROOTGLASS_DESKTOP_STATE_READY"
    )
    $associationDeadline = (Get-Date).AddMinutes(2)
    while ((Get-Date) -lt $associationDeadline) {
        $events = if (Test-Path -LiteralPath $associationLog) {
            Get-Content -LiteralPath $associationLog
        } else { @() }
        if (@($requiredMarkers | Where-Object { $events -notcontains $_ }).Count -eq 0) { break }
        Start-Sleep -Milliseconds 250
    }
    $events = if (Test-Path -LiteralPath $associationLog) { Get-Content -LiteralPath $associationLog } else { @() }
    foreach ($marker in $requiredMarkers) {
        if ($events -notcontains $marker) {
            throw "Windows file association did not complete '$marker'. Event log: $associationLog"
        }
    }
    @(
        "OS: $((Get-CimInstance Win32_OperatingSystem).Caption)",
        "OS version: $((Get-CimInstance Win32_OperatingSystem).Version)",
        "Build: $((Get-CimInstance Win32_OperatingSystem).BuildNumber)",
        "Runner architecture: $env:RUNNER_ARCH",
        "PROCESSOR_ARCHITECTURE: $env:PROCESSOR_ARCHITECTURE",
        "PROCESSOR_ARCHITEW6432: $env:PROCESSOR_ARCHITEW6432",
        "Installed executable: $RootglassExe",
        "Associated ProgID: $programId",
        "Association command: $openCommand",
        "Installer: $($installer[0].Name)",
        "Installer SHA-256: $installerHash",
        "Example: $($glass[0].Name)",
        "Example SHA-256: $glassHash",
        "QEMU: $qemuVersion"
    ) | Set-Content -LiteralPath (Join-Path $EvidenceDirectory "package-install.txt") -Encoding utf8
    Write-ProgressLine "Windows .glass association imported the signed archive"

    if (-not $RootglassProcess.HasExited) {
        if (-not $RootglassProcess.CloseMainWindow()) {
            Stop-Process -Id $RootglassProcess.Id -Force
        } elseif (-not $RootglassProcess.WaitForExit(10000)) {
            Stop-Process -Id $RootglassProcess.Id -Force
        }
    }
    $RootglassProcess = $null

    $firewallRootglass = "Rootglass-smoke-rootglass-$PID"
    $firewallQemu = "Rootglass-smoke-qemu-$PID"
    New-NetFirewallRule -DisplayName $firewallRootglass -Direction Outbound -Action Block `
        -Program $RootglassExe -RemoteAddress Internet -Profile Any | Out-Null
    $FirewallNames += $firewallRootglass
    New-NetFirewallRule -DisplayName $firewallQemu -Direction Outbound -Action Block `
        -Program $qemuExe -RemoteAddress Internet -Profile Any | Out-Null
    $FirewallNames += $firewallQemu
    "Blocked Internet egress for Rootglass and bundled QEMU; loopback remains available." |
        Set-Content -LiteralPath (Join-Path $EvidenceDirectory "offline-mode.txt") -Encoding utf8
    Write-ProgressLine "running the installed TUI guest workflow with Internet egress blocked"

    $qemuTemp = Join-Path $env:RUNNER_TEMP "rootglass-qemu-temp"
    New-Item -ItemType Directory -Force -Path $qemuTemp | Out-Null
    $env:TEMP = $qemuTemp
    $env:TMP = $qemuTemp
    $env:ROOTGLASS_QEMU_PATH = $qemuExe
    $tuiOutput = Join-Path $EvidenceDirectory "tui"
    $python = (Get-Command python -ErrorAction Stop).Source
    & $python -B $TuiSmokeScript $RootglassExe $glass[0].FullName $tuiOutput *> (Join-Path $EvidenceDirectory "tui-smoke-output.txt")
    if ($LASTEXITCODE -ne 0) {
        Get-Content -LiteralPath (Join-Path $EvidenceDirectory "tui-smoke-output.txt")
        throw "Installed TUI smoke exited with $LASTEXITCODE"
    }
    if (@(Get-RootglassQemuProcesses $qemuExe).Count -ne 0) {
        throw "A bundled QEMU process remained after the installed TUI exited."
    }
    Write-ProgressLine "installed TUI boot, login, restart, stop, and QEMU cleanup passed"

    $uninstallers = @(Get-ChildItem -LiteralPath $installRoot -Filter "unins*.exe" -File -Recurse -ErrorAction SilentlyContinue)
    if ($uninstallers.Count -ne 1) {
        throw "Expected one NSIS uninstaller under $installRoot; found $($uninstallers.Count)"
    }
    Write-ProgressLine "uninstalling Rootglass"
    $uninstall = Start-Process -FilePath $uninstallers[0].FullName -ArgumentList "/S" -Wait -PassThru
    if ($uninstall.ExitCode -ne 0) { throw "NSIS uninstaller exited with $($uninstall.ExitCode)" }
    if (Test-Path -LiteralPath $RootglassExe) { throw "Rootglass executable remains after uninstall" }
    if (-not (Test-Path -LiteralPath $trustPath -PathType Leaf)) { throw "Uninstall removed user trust configuration" }
    if (-not (Test-Path -LiteralPath (Join-Path $libraryRoot "registry.json") -PathType Leaf)) {
        throw "Uninstall removed the imported library registry"
    }
    $packageRows = @(Get-ChildItem -LiteralPath (Join-Path $libraryRoot "packages") -Directory -ErrorAction SilentlyContinue)
    if ($packageRows.Count -lt 1) { throw "Uninstall removed the imported package library" }
    if (-not (Test-Path -LiteralPath $stateRoot -PathType Container)) { throw "Guest state directory is missing after uninstall" }
    @(
        "Trust policy: retained",
        "Library registry: retained",
        "Imported packages: $($packageRows.Count)",
        "Guest state: retained"
    ) | Set-Content -LiteralPath (Join-Path $EvidenceDirectory "uninstall-retention.txt") -Encoding utf8
    Write-ProgressLine "uninstall preserved trust, imported package, registry, and guest state"
}
finally {
    foreach ($name in $FirewallNames) {
        Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
    }
    if ($qemuExe -and (Test-Path -LiteralPath $qemuExe)) {
        $remainingQemu = @(Get-RootglassQemuProcesses $qemuExe)
        if ($remainingQemu.Count -gt 0) {
            $remainingQemu | ForEach-Object {
                Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
            }
            $remainingQemu | Format-List ProcessId, ExecutablePath, CommandLine |
                Out-File -FilePath (Join-Path $EvidenceDirectory "cleanup-orphaned-qemu.txt") -Encoding utf8
        }
    }
    if ($RootglassProcess -and -not $RootglassProcess.HasExited) {
        try { $RootglassProcess.CloseMainWindow() | Out-Null } catch {}
        if (-not $RootglassProcess.WaitForExit(3000)) {
            Stop-Process -Id $RootglassProcess.Id -Force -ErrorAction SilentlyContinue
        }
    }
}
