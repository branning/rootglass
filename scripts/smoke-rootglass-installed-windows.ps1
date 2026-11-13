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
using System.Text;
public static class RootglassSmokeWindow {
    private const int SW_MAXIMIZE = 3;
    private const int SW_RESTORE = 9;

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern IntPtr SetActiveWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr window, StringBuilder text, int maxCount);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassName(IntPtr window, StringBuilder className, int maxCount);
    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    private static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
    [DllImport("user32.dll")]
    private static extern bool ShowWindowAsync(IntPtr window, int command);
    [DllImport("user32.dll")]
    private static extern bool BringWindowToTop(IntPtr window);
    [DllImport("user32.dll")]
    private static extern IntPtr SetFocus(IntPtr window);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool AttachThreadInput(uint attach, uint attachTo, bool attachInput);

    [StructLayout(LayoutKind.Sequential)]
    private struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
    [DllImport("user32.dll")]
    private static extern bool GetClientRect(IntPtr window, out RECT rect);
    [DllImport("user32.dll")]
    private static extern bool ClientToScreen(IntPtr window, ref POINT point);
    [StructLayout(LayoutKind.Sequential)]
    private struct POINT {
        public int X;
        public int Y;
        public POINT(int x, int y) { X = x; Y = y; }
    }

    public static bool ActivateWindow(IntPtr window) {
        if (window == IntPtr.Zero) return false;
        uint currentThread = GetCurrentThreadId();
        IntPtr foreground = GetForegroundWindow();
        uint foregroundThread = foreground == IntPtr.Zero ? 0 : GetWindowThreadProcessId(foreground, out _);
        uint targetThread = GetWindowThreadProcessId(window, out _);
        bool attachedForeground = false;
        bool attachedTarget = false;
        try {
            if (foregroundThread != 0 && foregroundThread != currentThread) {
                attachedForeground = AttachThreadInput(currentThread, foregroundThread, true);
            }
            if (targetThread != 0 && targetThread != currentThread && targetThread != foregroundThread) {
                attachedTarget = AttachThreadInput(currentThread, targetThread, true);
            }
            ShowWindowAsync(window, SW_RESTORE);
            ShowWindowAsync(window, SW_MAXIMIZE);
            BringWindowToTop(window);
            for (int attempt = 0; attempt < 10; attempt++) {
                SetForegroundWindow(window);
                SetActiveWindow(window);
                SetFocus(window);
                if (GetForegroundWindow() == window) return true;
                System.Threading.Thread.Sleep(100);
            }
            return GetForegroundWindow() == window;
        } finally {
            if (attachedTarget) AttachThreadInput(currentThread, targetThread, false);
            if (attachedForeground) AttachThreadInput(currentThread, foregroundThread, false);
        }
    }

    public static string ClickAt(int x, int y) {
        if (!SetCursorPos(x, y)) {
            throw new InvalidOperationException("Could not position the pointer at " + x + "," + y + ".");
        }
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(50);
        mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
        return x + "," + y;
    }

    public static string ClickImportGlass(IntPtr window) {
        RECT client;
        if (window == IntPtr.Zero || !GetClientRect(window, out client)) {
            throw new InvalidOperationException("Could not read the Rootglass client area for the Import Glass fallback.");
        }
        POINT point = new POINT(client.Right - 88, 82);
        if (!ClientToScreen(window, ref point)) {
            throw new InvalidOperationException("Could not map the Import Glass button location to screen coordinates.");
        }
        // The import button is the rightmost control in the library header.
        return ClickAt(point.X, point.Y);
    }

    public static string DescribeWindow(IntPtr window) {
        StringBuilder title = new StringBuilder(512);
        StringBuilder className = new StringBuilder(256);
        GetWindowText(window, title, title.Capacity);
        GetClassName(window, className, className.Capacity);
        return "title='" + title + "' class='" + className + "' handle=" + window;
    }
}
"@
    $pickerLog = Join-Path $EvidenceDirectory "picker-events.log"
    $env:ROOTGLASS_DESKTOP_SMOKE = "1"
    $env:ROOTGLASS_DESKTOP_SMOKE_EVENT_LOG = $pickerLog
    $env:ROOTGLASS_DESKTOP_SMOKE_EXPECTED_TRUST_KEY_ID = "ed25519:4a079f6f512c9e1a24f83b08e84fb8508996c91e0d54a19512fabfafc82b62d0"
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    function Get-WindowsPrivacyOobePage {
        $desktop = [System.Windows.Automation.AutomationElement]::RootElement
        $windows = $desktop.FindAll(
            [System.Windows.Automation.TreeScope]::Children,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($candidate in $windows) {
            try {
                $controls = $candidate.FindAll(
                    [System.Windows.Automation.TreeScope]::Descendants,
                    [System.Windows.Automation.Condition]::TrueCondition
                )
                $names = @()
                foreach ($control in $controls) {
                    try {
                        $name = $control.Current.Name
                        if ($name) { $names += $name }
                    } catch { }
                }
                if (($names -join " ") -match '(?i)choose privacy settings for your device') {
                    return [pscustomobject]@{
                        Window = $candidate
                        Controls = @($controls)
                        Summary = $names -join "; "
                    }
                }
            } catch { }
        }
        return $null
    }
    function Complete-WindowsPrivacyOobe {
        $attempts = 0
        while ($attempts -lt 3) {
            $page = Get-WindowsPrivacyOobePage
            if ($null -eq $page) {
                if ($attempts -gt 0) { Write-ProgressLine "Windows privacy first-run setup completed" }
                return
            }
            Write-ProgressLine "Windows privacy first-run page detected; disabling optional settings before continuing"
            $disabledToggles = 0
            foreach ($control in $page.Controls) {
                try {
                    $toggle = $control.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
                    if ($toggle.Current.ToggleState -eq [System.Windows.Automation.ToggleState]::On) {
                        $toggle.Toggle()
                        $disabledToggles++
                    }
                } catch { }
            }
            $nextButton = $null
            foreach ($control in $page.Controls) {
                try {
                    if ($control.Current.ControlType -eq [System.Windows.Automation.ControlType]::Button -and
                        $control.Current.Name -match '^(Next|Accept)$') {
                        $nextButton = $control
                        break
                    }
                } catch { }
            }
            if ($null -ne $nextButton) {
                try {
                    $invoke = $nextButton.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
                    $invoke.Invoke()
                    Write-ProgressLine "advanced the Windows privacy page through UI Automation; disabled $disabledToggles optional toggles"
                } catch {
                    [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
                    Write-ProgressLine "UI Automation could not activate Next; sent Enter after disabling $disabledToggles optional toggles"
                }
            } else {
                $oobeHandle = [IntPtr]$page.Window.Current.NativeWindowHandle
                if ($oobeHandle -ne [IntPtr]::Zero) {
                    [void][RootglassSmokeWindow]::ActivateWindow($oobeHandle)
                }
                [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
                Write-ProgressLine "advanced the detected Windows privacy page with Enter; disabled $disabledToggles optional toggles"
            }
            $attempts++
            Start-Sleep -Seconds 2
        }
        $remainingPage = Get-WindowsPrivacyOobePage
        if ($null -ne $remainingPage) {
            throw "Windows first-run privacy page remained after three attempts: $($remainingPage.Summary)"
        }
    }
    Write-ProgressLine "opening the example through the GPUI file picker"
    Complete-WindowsPrivacyOobe
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
    $foregroundSet = [RootglassSmokeWindow]::ActivateWindow($RootglassProcess.MainWindowHandle)
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
    if ($foregroundWindow -ne $RootglassProcess.MainWindowHandle) {
        throw "Could not bring installed Rootglass to the foreground. $([RootglassSmokeWindow]::DescribeWindow($foregroundWindow))"
    }
    $importMethod = ""
    $importBounds = $null
    try {
        $windowAutomation = [System.Windows.Automation.AutomationElement]::FromHandle($RootglassProcess.MainWindowHandle)
        $elements = $windowAutomation.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition
        )
        foreach ($element in $elements) {
            if ($element.Current.Name -match '(?i)import glass|open.*bundle' -or
                $element.Current.AutomationId -ieq 'open-glass') {
                try {
                    $invoke = $element.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
                    $invoke.Invoke()
                    $importMethod = "UI Automation InvokePattern"
                    break
                } catch {
                    Write-ProgressLine "matching UI Automation element had no usable invoke pattern: $($_.Exception.Message)"
                    $importBounds = $element.Current.BoundingRectangle
                }
            }
        }
    } catch {
        Write-ProgressLine "UI Automation lookup failed: $($_.Exception.Message)"
    }
    if (-not $importMethod) {
        if ($null -ne $importBounds -and $importBounds.Width -gt 0 -and $importBounds.Height -gt 0) {
            $x = [int]($importBounds.Left + $importBounds.Width / 2)
            $y = [int]($importBounds.Top + $importBounds.Height / 2)
            $clickPoint = [RootglassSmokeWindow]::ClickAt($x, $y)
            $importMethod = "mouse click on the accessible Import Glass bounds at $clickPoint"
        } else {
            $clickPoint = [RootglassSmokeWindow]::ClickImportGlass($RootglassProcess.MainWindowHandle)
            $importMethod = "mouse click on the library header Import Glass location at $clickPoint"
        }
    }
    Write-ProgressLine "activated Import Glass with $importMethod"
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
    Start-Sleep -Milliseconds 1000
    try {
        Add-Type -AssemblyName System.Drawing
        $screenBounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($screenBounds.Width, $screenBounds.Height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($screenBounds.Location, [System.Drawing.Point]::Empty, $screenBounds.Size)
        $bitmap.Save((Join-Path $EvidenceDirectory "desktop-picker-open.png"), [System.Drawing.Imaging.ImageFormat]::Png)
        $graphics.Dispose()
        $bitmap.Dispose()
    } catch {
        Write-ProgressLine "picker-open screenshot capture failed: $($_.Exception.Message)"
    }
    $dialogAutomation = $null
    $pickerControls = $null
    $fileNameControl = $null
    $openButton = $null
    $topLevelWindows = @()
    $dialogDeadline = (Get-Date).AddSeconds(15)
    while ((Get-Date) -lt $dialogDeadline) {
        try {
            $topLevelWindows = @()
            $desktopAutomation = [System.Windows.Automation.AutomationElement]::RootElement
            $windows = $desktopAutomation.FindAll(
                [System.Windows.Automation.TreeScope]::Children,
                [System.Windows.Automation.Condition]::TrueCondition
            )
            foreach ($candidate in $windows) {
                try {
                    $current = $candidate.Current
                    $windowHandle = [IntPtr]$current.NativeWindowHandle
                    if ($windowHandle -eq [IntPtr]::Zero -or
                        $windowHandle -eq $RootglassProcess.MainWindowHandle) { continue }

                    $candidateControls = $candidate.FindAll(
                        [System.Windows.Automation.TreeScope]::Descendants,
                        [System.Windows.Automation.Condition]::TrueCondition
                    )
                    $candidateFileName = $null
                    $candidateOpenButton = $null
                    $candidateControlNames = @()
                    foreach ($control in $candidateControls) {
                        try {
                            $controlCurrent = $control.Current
                            if ($controlCurrent.ControlType -eq [System.Windows.Automation.ControlType]::Edit -or
                                $controlCurrent.ControlType -eq [System.Windows.Automation.ControlType]::Button) {
                                $candidateControlNames += "$($controlCurrent.ControlType.ProgrammaticName) name='$($controlCurrent.Name)' id='$($controlCurrent.AutomationId)'"
                            }
                            if ($null -eq $candidateFileName -and
                                $controlCurrent.ControlType -eq [System.Windows.Automation.ControlType]::Edit -and
                                ($controlCurrent.Name -match '(?i)file name' -or $controlCurrent.AutomationId -eq '1148')) {
                                $candidateFileName = $control
                            }
                            if ($null -eq $candidateOpenButton -and
                                $controlCurrent.ControlType -eq [System.Windows.Automation.ControlType]::Button -and
                                $controlCurrent.Name -match '^(Open|Select)(\b.*)?$') {
                                $candidateOpenButton = $control
                            }
                        } catch { }
                    }
                    $description = "name='$($current.Name)' class='$($current.ClassName)' handle=$windowHandle pid=$($current.ProcessId) controls=[$($candidateControlNames -join '; ')]"
                    $topLevelWindows += $description
                    $pickerTitle = $current.Name -match '^(Open|Select|Choose|Pick)(\b.*)?$'
                    if ($null -ne $candidateOpenButton -and
                        ($null -ne $candidateFileName -or $pickerTitle -or $current.ClassName -eq '#32770')) {
                        $dialogAutomation = $candidate
                        $pickerControls = $candidateControls
                        $fileNameControl = $candidateFileName
                        $openButton = $candidateOpenButton
                        break
                    }
                } catch { }
            }
        } catch {
            Write-ProgressLine "native picker UI Automation search failed: $($_.Exception.Message)"
        }
        if ($null -ne $dialogAutomation) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($null -eq $dialogAutomation) {
        try {
            Add-Type -AssemblyName System.Drawing
            $screenBounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
            $bitmap = [System.Drawing.Bitmap]::new($screenBounds.Width, $screenBounds.Height)
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            $graphics.CopyFromScreen($screenBounds.Location, [System.Drawing.Point]::Empty, $screenBounds.Size)
            $bitmap.Save((Join-Path $EvidenceDirectory "desktop-picker-uia-unavailable.png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $graphics.Dispose()
            $bitmap.Dispose()
        } catch {
            Write-ProgressLine "picker-timeout screenshot capture failed: $($_.Exception.Message)"
        }
        $activeDialogHandle = [RootglassSmokeWindow]::GetForegroundWindow()
        $activeDialogDescription = [RootglassSmokeWindow]::DescribeWindow($activeDialogHandle)
        Write-ProgressLine "UI Automation did not enumerate the picker; foreground window: $activeDialogDescription"
        if ($activeDialogDescription -notmatch "title='(Open|Select|Choose|Pick)(\b[^']*)?'" -and
            $activeDialogDescription -notmatch "class='#32770'") {
            throw "The picker-open event was recorded, but the foreground window is not a native file dialog: $activeDialogDescription. Top-level windows: $($topLevelWindows -join '; ')"
        }

        # IFileOpenDialog is visibly modal on some hosted Windows runners but is
        # absent from the UIA desktop tree. Send the filename to that verified
        # foreground dialog, then rely on Rootglass's own import markers below.
        [System.Windows.Forms.SendKeys]::SendWait("%n")
        [System.Windows.Forms.SendKeys]::SendWait($glass[0].FullName)
        [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
        Write-ProgressLine "selected the example through the foreground native dialog keyboard path"
        $pickerControls = @()
        $fileNameControl = $null
        $openButton = $null
        $dialogAutomation = $null
    }
    if ($null -ne $dialogAutomation) {
        $dialogHandle = [IntPtr]$dialogAutomation.Current.NativeWindowHandle
        $dialogForeground = [RootglassSmokeWindow]::SetForegroundWindow($dialogHandle)
        [void][RootglassSmokeWindow]::SetActiveWindow($dialogHandle)
        Start-Sleep -Milliseconds 500
        Write-ProgressLine "focused native picker '$($dialogAutomation.Current.Name)' handle=$dialogHandle foreground=$dialogForeground"
        try {
            Add-Type -AssemblyName System.Drawing
            $screenBounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
            $bitmap = [System.Drawing.Bitmap]::new($screenBounds.Width, $screenBounds.Height)
            $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
            $graphics.CopyFromScreen($screenBounds.Location, [System.Drawing.Point]::Empty, $screenBounds.Size)
            $bitmap.Save((Join-Path $EvidenceDirectory "desktop-native-picker.png"), [System.Drawing.Imaging.ImageFormat]::Png)
            $graphics.Dispose()
            $bitmap.Dispose()
        } catch {
            Write-ProgressLine "native picker screenshot capture failed: $($_.Exception.Message)"
        }

        $fileNameSet = $false
        $controlDescriptions = @()
        foreach ($control in $pickerControls) {
            try {
                $current = $control.Current
                if ($current.ControlType -eq [System.Windows.Automation.ControlType]::Edit -or
                    $current.ControlType -eq [System.Windows.Automation.ControlType]::Button) {
                    $controlDescriptions += "$($current.ControlType.ProgrammaticName) name='$($current.Name)' id='$($current.AutomationId)'"
                }
            } catch { }
        }
        Write-ProgressLine "native picker controls: $($controlDescriptions -join '; ')"
        if ($null -ne $fileNameControl) {
            try {
                $valuePattern = $fileNameControl.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
                $valuePattern.SetValue($glass[0].FullName)
                $fileNameSet = $true
            } catch {
                Write-ProgressLine "file-name UI Automation value pattern failed: $($_.Exception.Message)"
            }
        }
        if ($fileNameSet -and $null -ne $openButton) {
            try {
                $openInvoke = $openButton.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
                $openInvoke.Invoke()
                Write-ProgressLine "selected the example through UI Automation"
            } catch {
                Write-ProgressLine "native picker Open button invoke failed; using keyboard fallback: $($_.Exception.Message)"
                [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
            }
        } else {
            [System.Windows.Forms.SendKeys]::SendWait("%n")
            [System.Windows.Forms.SendKeys]::SendWait($glass[0].FullName)
            [System.Windows.Forms.SendKeys]::SendWait("{ENTER}")
            Write-ProgressLine "selected the example through native dialog keyboard input"
        }
    }
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
