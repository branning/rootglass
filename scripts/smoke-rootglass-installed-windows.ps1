param(
    [Parameter(Mandatory = $true)][string]$PackageDirectory,
    [Parameter(Mandatory = $true)][string]$ExampleDirectory,
    [Parameter(Mandatory = $true)][string]$EvidenceDirectory,
    [Parameter(Mandatory = $true)][string]$ChecksumName,
    [Parameter(Mandatory = $true)][ValidateSet("windows-x86_64", "windows-aarch64")][string]$ExpectedRuntimePlatform,
    [Parameter(Mandatory = $true)][string]$TuiSmokeScript,
    [bool]$ProbeQemuStartupOnly = $false
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

function Get-PeMachine([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = $null
    try {
        $reader = [IO.BinaryReader]::new($stream)
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw "Not a Windows PE executable: $Path" }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadInt32()
        if ($peOffset -lt 0 -or $peOffset + 6 -gt $stream.Length) { throw "Invalid PE header: $Path" }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "Invalid PE signature: $Path" }
        return $reader.ReadUInt16()
    }
    finally {
        if ($reader) { $reader.Dispose() } else { $stream.Dispose() }
    }
}

function Get-RootglassQemuProcesses([string]$QemuPath) {
    $env:ROOTGLASS_QEMU_PATH = $QemuPath
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='qemu-system-aarch64.exe'" |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $env:ROOTGLASS_QEMU_PATH })
    Remove-Item Env:\ROOTGLASS_QEMU_PATH -ErrorAction SilentlyContinue
    return $processes
}

function Invoke-QemuStartupProbe([string]$QemuPath, [string]$RuntimeRoot, [string]$EvidenceDirectory, [string]$ExpectedRuntimePlatform, [string]$Msys2Root) {
    $probeRoot = Join-Path $EvidenceDirectory "qemu-startup-probes"
    New-Item -ItemType Directory -Force -Path $probeRoot | Out-Null
    $probeResults = @()
    $dataDirectory = Join-Path $RuntimeRoot "share\qemu"
    $moduleDirectory = Join-Path $RuntimeRoot "lib\qemu"
    $msysHome = $null
    if ($ExpectedRuntimePlatform -eq "windows-aarch64" -and -not [string]::IsNullOrWhiteSpace($Msys2Root)) {
        $bashPath = Join-Path $Msys2Root "usr\bin\bash.exe"
        if (-not (Test-Path -LiteralPath $bashPath -PathType Leaf)) {
            throw "MSYS2 bash was not found at $bashPath"
        }
        $msysHome = (& $bashPath -lc 'printf "%s\n" "$HOME"' | Select-Object -First 1).Trim()
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($msysHome)) {
            throw "Could not resolve HOME from the MSYS2 CLANGARM64 login environment"
        }
    }

    $repositoryRoot = Split-Path -Parent $PSScriptRoot
    $powershellVersionScript = Join-Path $probeRoot "invoke-qemu-version.ps1"
    @'
param([string]$QemuPath)
$versionOutput = & $QemuPath --version
$versionOutput | Set-Content -LiteralPath $env:ROOTGLASS_QEMU_VERSION_OUTPUT -Encoding utf8
exit $LASTEXITCODE
'@ | Set-Content -LiteralPath $powershellVersionScript -Encoding utf8

    $probeNames = if ($ExpectedRuntimePlatform -eq "windows-aarch64" -and -not [string]::IsNullOrWhiteSpace($Msys2Root)) {
        @(
            "version-msys2-env",
            "machine-none-msys2-env",
            "machine-virt-bundle-path-msys2-env",
            "serial-stdio-bundle-path-msys2-env",
            "machine-virt-mingw-path-msys2-env",
            "machine-virt-usr-path-msys2-env",
            "machine-virt-msys2-env",
            "serial-stdio-msys2-env"
        )
    } else {
        @("version-piped", "version-inherited", "version-repository-cwd", "version-powershell", "version-msys2-env", "version-system-path-only", "machine-none", "machine-none-system-path-only", "serial-null", "serial-stdio")
    }
    foreach ($probeName in $probeNames) {
        $serialBackend = if ($probeName -like "serial-*") {
            $probeName -replace "^serial-", "" -replace "-bundle-path-msys2-env$", "" -replace "-msys2-env$", ""
        } elseif ($probeName -like "machine-virt-*") {
            "null"
        } else { "not applicable" }
        $qmpEnabled = $probeName -notlike "version-*"
        $redirectOutput = $probeName -ne "version-inherited"
        $port = $null
        if ($qmpEnabled) {
            $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
            $listener.Start()
            $port = ([Net.IPEndPoint]$listener.LocalEndpoint).Port
            $listener.Stop()
        }

        $stdoutPath = Join-Path $probeRoot "$probeName.stdout.txt"
        $stderrPath = Join-Path $probeRoot "$probeName.stderr.txt"
        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = if ($probeName -eq "version-powershell") {
            Join-Path $PSHOME "pwsh.exe"
        } else { $QemuPath }
        $startInfo.WorkingDirectory = if ($probeName -eq "version-repository-cwd") {
            $repositoryRoot
        } else { Split-Path -Parent $QemuPath }
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $redirectOutput
        $startInfo.RedirectStandardInput = $redirectOutput
        $startInfo.RedirectStandardOutput = $redirectOutput
        $startInfo.RedirectStandardError = $redirectOutput
        if ($probeName -like "*-system-path-only") {
            $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$(Join-Path $env:SystemRoot 'System32');$env:SystemRoot"
            foreach ($name in @("MSYSTEM", "MINGW_PREFIX", "MINGW_CHOST", "MINGW_PACKAGE_PREFIX", "MSYSTEM_CARCH", "MSYSTEM_CHOST", "MSYSTEM_PREFIX", "MSYSTEM_PATH", "MSYS", "MSYS2_PATH_TYPE", "MSYS2_ARG_CONV_EXCL", "CHERE_INVOKING")) {
                [void]$startInfo.Environment.Remove($name)
            }
        } else {
            $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$env:PATH"
        }
        if (Test-Path -LiteralPath $moduleDirectory -PathType Container) {
            $startInfo.Environment["QEMU_MODULE_DIR"] = $moduleDirectory
        }
        if ($probeName -eq "version-powershell") {
            $versionOutputPath = Join-Path $probeRoot "version-powershell-native.stdout.txt"
            $startInfo.Environment["ROOTGLASS_QEMU_VERSION_OUTPUT"] = $versionOutputPath
        }
        if ($probeName -like "*-msys2-env") {
            $startInfo.Environment["MSYSTEM"] = "CLANGARM64"
            $startInfo.Environment["MINGW_PREFIX"] = "/clangarm64"
            $startInfo.Environment["MSYSTEM_PREFIX"] = "/clangarm64"
            $startInfo.Environment["MINGW_CHOST"] = "aarch64-w64-mingw32"
            $startInfo.Environment["MSYSTEM_CARCH"] = "aarch64"
            $startInfo.Environment["MSYSTEM_CHOST"] = "aarch64-w64-mingw32"
            $startInfo.Environment["MSYS2_PATH_TYPE"] = "inherit"
            [void]$startInfo.Environment.Remove("CHERE_INVOKING")
            if (-not [string]::IsNullOrWhiteSpace($Msys2Root)) {
                $startInfo.Environment["HOME"] = $msysHome
                if ($probeName -like "*-bundle-path-msys2-env") {
                    $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$env:PATH"
                } elseif ($probeName -like "*-mingw-path-msys2-env") {
                    $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$(Join-Path $Msys2Root 'clangarm64\bin');$env:PATH"
                } elseif ($probeName -like "*-usr-path-msys2-env") {
                    $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$(Join-Path $Msys2Root 'usr\bin');$env:PATH"
                } else {
                    $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$(Join-Path $Msys2Root 'clangarm64\bin');$(Join-Path $Msys2Root 'usr\bin');$env:PATH"
                }
            }
            else {
                $startInfo.Environment["HOME"] = $env:USERPROFILE
            }
        }
        $arguments = switch ($probeName) {
            "version-piped" { @("--version") }
            "version-inherited" { @("--version") }
            "version-repository-cwd" { @("--version") }
            "version-powershell" { @("-NoLogo", "-NoProfile", "-NonInteractive", "-File", $powershellVersionScript, $QemuPath) }
            "version-msys2-env" { @("--version") }
            "version-system-path-only" { @("--version") }
            "machine-none" { @(
                "-L", $dataDirectory,
                "-machine", "none",
                "-display", "none",
                "-monitor", "none",
                "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                "-S"
            ) }
            "machine-none-system-path-only" { @(
                "-L", $dataDirectory,
                "-machine", "none",
                "-display", "none",
                "-monitor", "none",
                "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                "-S"
            ) }
            "machine-none-msys2-env" { @(
                "-L", $dataDirectory,
                "-machine", "none",
                "-display", "none",
                "-monitor", "none",
                "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                "-S"
            ) }
            { $_ -like "machine-virt-*-msys2-env" -or $_ -eq "machine-virt-msys2-env" } { @(
                "-L", $dataDirectory,
                "-machine", "virt",
                "-cpu", "cortex-a57",
                "-m", "512",
                "-smp", "1",
                "-display", "none",
                "-serial", $serialBackend,
                "-monitor", "none",
                "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                "-S"
            ) }
            { $_ -like "serial-*-msys2-env" } { @(
                "-L", $dataDirectory,
                "-machine", "virt",
                "-cpu", "cortex-a57",
                "-m", "512",
                "-smp", "1",
                "-display", "none",
                "-serial", $serialBackend,
                "-monitor", "none",
                "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                "-S"
            ) }
            default {
                @(
                    "-L", $dataDirectory,
                    "-machine", "virt",
                    "-cpu", "cortex-a57",
                    "-m", "512",
                    "-smp", "1",
                    "-display", "none",
                    "-serial", $serialBackend,
                    "-monitor", "none",
                    "-qmp", "tcp:127.0.0.1:$port,server=on,wait=off",
                    "-S"
                )
            }
        }
        foreach ($argument in $arguments) {
            $startInfo.ArgumentList.Add($argument)
        }

        $process = [Diagnostics.Process]::new()
        $process.StartInfo = $startInfo
        $processStarted = $false
        $stdoutReader = $null
        $stderrReader = $null
        $qmpGreeting = "not received"
        $qmpCapabilities = "not received"
        $probeStatus = if ($qmpEnabled) { "no QMP listener within 15 seconds" } else { "QEMU did not exit within 10 seconds" }
        try {
            if (-not $process.Start()) { throw "Process.Start returned false" }
            $processStarted = $true
            if ($redirectOutput) {
                $stdoutReader = $process.StandardOutput.ReadToEndAsync()
                $stderrReader = $process.StandardError.ReadToEndAsync()
            }
            if ($probeName -eq "machine-none" -or $probeName -like "machine-virt-*") {
                Start-Sleep -Milliseconds 1000
                $moduleSnapshot = Join-Path $probeRoot "$probeName.modules.txt"
                try {
                    @(Get-Process -Id $process.Id -ErrorAction Stop | ForEach-Object {
                        $_.Modules | ForEach-Object { "$($_.ModuleName)`t$($_.FileName)" }
                    }) | Set-Content -LiteralPath $moduleSnapshot -Encoding utf8
                } catch {
                    "module enumeration failed: $($_.Exception.Message)" | Set-Content -LiteralPath $moduleSnapshot -Encoding utf8
                }
            }
            if (-not $qmpEnabled) {
                if ($process.WaitForExit(10000)) {
                    $probeStatus = "QEMU --version exited with code $($process.ExitCode)"
                }
            } else {
                $deadline = (Get-Date).AddSeconds(15)
                while ((Get-Date) -lt $deadline -and -not $process.HasExited) {
                    $qemuListener = @(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue |
                        Where-Object { $_.OwningProcess -eq $process.Id })
                    if ($qemuListener.Count -gt 0) {
                        $client = [Net.Sockets.TcpClient]::new()
                        try {
                            $client.ReceiveTimeout = 3000
                            $client.SendTimeout = 3000
                            $client.Connect([Net.IPAddress]::Loopback, $port)
                            $stream = $client.GetStream()
                            $reader = [IO.StreamReader]::new($stream, [Text.Encoding]::ASCII, $false, 1024, $true)
                            $qmpGreeting = $reader.ReadLine()
                            $request = [Text.Encoding]::ASCII.GetBytes('{"execute":"qmp_capabilities"}' + "`r`n")
                            $stream.Write($request, 0, $request.Length)
                            $qmpCapabilities = $reader.ReadLine()
                            $probeStatus = "QMP greeting and capabilities exchange passed"
                        } catch {
                            $probeStatus = "QMP listener opened, protocol exchange failed: $($_.Exception.Message)"
                        } finally {
                            if ($client) { $client.Dispose() }
                        }
                        break
                    }
                    Start-Sleep -Milliseconds 250
                }
                if ($process.HasExited) {
                    $process.Refresh()
                    $probeStatus = "QEMU exited before QMP opened with code $($process.ExitCode)"
                }
            }
        } catch {
            $probeStatus = "probe could not start QEMU: $($_.Exception.Message)"
        } finally {
            if ($processStarted -and -not $process.HasExited) {
                if ($probeName -eq "version-powershell") { $process.Kill($true) } else { $process.Kill() }
                $process.WaitForExit()
            }
            if ($stdoutReader) { $stdoutReader.GetAwaiter().GetResult() | Set-Content -LiteralPath $stdoutPath -Encoding utf8 }
            if ($stderrReader) { $stderrReader.GetAwaiter().GetResult() | Set-Content -LiteralPath $stderrPath -Encoding utf8 }
            $probeResults += [pscustomobject]@{
                ProbeName = $probeName
                SerialBackend = $serialBackend
                QmpPort = $port
                Status = $probeStatus
                QmpGreeting = $qmpGreeting
                QmpCapabilities = $qmpCapabilities
                ExitCode = if ($processStarted -and $process.HasExited) { $process.ExitCode } else { "not started" }
                StandardOutput = if ($redirectOutput) { $stdoutPath } else { "inherited to workflow log" }
                StandardError = if ($redirectOutput) { $stderrPath } else { "inherited to workflow log" }
                WorkingDirectory = $startInfo.WorkingDirectory
                Executable = $startInfo.FileName
            }
            $process.Dispose()
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($Msys2Root)) {
        $probeName = "version-msys2-login-shell"
        $stdoutPath = Join-Path $probeRoot "$probeName.stdout.txt"
        $stderrPath = Join-Path $probeRoot "$probeName.stderr.txt"
        $bashPath = Join-Path $Msys2Root "usr\bin\bash.exe"
        $cygpathPath = Join-Path $Msys2Root "usr\bin\cygpath.exe"
        $probeStatus = "MSYS2 bash or cygpath was not found"
        $exitCode = "not started"
        $process = $null
        $stdoutReader = $null
        $stderrReader = $null
        try {
            if (-not (Test-Path -LiteralPath $bashPath -PathType Leaf)) { throw "MSYS2 bash was not found at $bashPath" }
            if (-not (Test-Path -LiteralPath $cygpathPath -PathType Leaf)) { throw "MSYS2 cygpath was not found at $cygpathPath" }
            $posixQemuPath = (& $cygpathPath -u $QemuPath | Select-Object -First 1).Trim()
            if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($posixQemuPath)) {
                throw "Could not convert the installed QEMU path to an MSYS2 path"
            }

            $startInfo = [Diagnostics.ProcessStartInfo]::new()
            $startInfo.FileName = $bashPath
            $startInfo.WorkingDirectory = Split-Path -Parent $QemuPath
            $startInfo.UseShellExecute = $false
            $startInfo.CreateNoWindow = $true
            $startInfo.RedirectStandardInput = $true
            $startInfo.RedirectStandardOutput = $true
            $startInfo.RedirectStandardError = $true
            $startInfo.Environment["MSYSTEM"] = "CLANGARM64"
            $startInfo.Environment["MSYS2_PATH_TYPE"] = "inherit"
            $startInfo.Environment["CHERE_INVOKING"] = "1"
            $startInfo.Environment["ROOTGLASS_QEMU_EXE"] = $posixQemuPath
            $startInfo.Environment["PATH"] = "$(Split-Path -Parent $QemuPath);$(Join-Path $Msys2Root 'clangarm64\bin');$(Join-Path $Msys2Root 'usr\bin');$env:PATH"
            if (Test-Path -LiteralPath $moduleDirectory -PathType Container) {
                $startInfo.Environment["QEMU_MODULE_DIR"] = $moduleDirectory
            }
            $startInfo.ArgumentList.Add("-lc")
            $startInfo.ArgumentList.Add('printf "MSYSTEM=%s MINGW_PREFIX=%s MSYSTEM_PREFIX=%s MINGW_CHOST=%s MSYSTEM_CARCH=%s MSYSTEM_CHOST=%s MSYS2_PATH_TYPE=%s CHERE_INVOKING=%s HOME=%s\\n" "$MSYSTEM" "$MINGW_PREFIX" "$MSYSTEM_PREFIX" "$MINGW_CHOST" "$MSYSTEM_CARCH" "$MSYSTEM_CHOST" "$MSYS2_PATH_TYPE" "$CHERE_INVOKING" "$HOME"; "$ROOTGLASS_QEMU_EXE" --version')

            $process = [Diagnostics.Process]::new()
            $process.StartInfo = $startInfo
            if (-not $process.Start()) { throw "Could not start MSYS2 bash" }
            $stdoutReader = $process.StandardOutput.ReadToEndAsync()
            $stderrReader = $process.StandardError.ReadToEndAsync()
            if ($process.WaitForExit(20000)) {
                $probeStatus = "MSYS2 shell QEMU --version exited with code $($process.ExitCode)"
                $exitCode = $process.ExitCode
            } else {
                $probeStatus = "MSYS2 shell QEMU --version did not exit within 20 seconds"
                $process.Kill($true)
                $process.WaitForExit()
                $exitCode = $process.ExitCode
            }
        } catch {
            $probeStatus = "MSYS2 shell probe failed: $($_.Exception.Message)"
        } finally {
            if ($process -and -not $process.HasExited) {
                $process.Kill($true)
                $process.WaitForExit()
            }
            if ($stdoutReader) { $stdoutReader.GetAwaiter().GetResult() | Set-Content -LiteralPath $stdoutPath -Encoding utf8 }
            if ($stderrReader) { $stderrReader.GetAwaiter().GetResult() | Set-Content -LiteralPath $stderrPath -Encoding utf8 }
            if ($process) { $process.Dispose() }
        }
        $probeResults += [pscustomobject]@{
            ProbeName = $probeName
            SerialBackend = "MSYS2 login shell"
            QmpPort = $null
            Status = $probeStatus
            QmpGreeting = "not received"
            QmpCapabilities = "not received"
            ExitCode = $exitCode
            StandardOutput = $stdoutPath
            StandardError = $stderrPath
            WorkingDirectory = Split-Path -Parent $QemuPath
            Executable = $bashPath
        }
    }
    $probeResults | Format-List | Out-String | Set-Content -LiteralPath (Join-Path $probeRoot "results.txt") -Encoding utf8
    return $probeResults
}

try {
    $progress = Join-Path $EvidenceDirectory "progress.txt"
    "Windows installed package smoke" | Set-Content -LiteralPath $progress -Encoding utf8
    Write-ProgressLine "verifying package and signed example checksums"
    Verify-Checksums $PackageDirectory $ChecksumName
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
    if ($runtimeInfo.target_platform -ne $ExpectedRuntimePlatform) {
        throw "Expected $ExpectedRuntimePlatform runtime, found $($runtimeInfo.target_platform)"
    }
    $expectedMachine = if ($ExpectedRuntimePlatform -eq "windows-aarch64") { 0xAA64 } else { 0x8664 }
    $rootglassMachine = Get-PeMachine $RootglassExe
    $qemuExe = Join-Path $versionRoot "bin\qemu-system-aarch64.exe"
    $qemuMachine = Get-PeMachine $qemuExe
    if ($rootglassMachine -ne $expectedMachine) {
        throw ("Expected Rootglass PE machine 0x{0:X4}, found 0x{1:X4}" -f $expectedMachine, $rootglassMachine)
    }
    if ($qemuMachine -ne $expectedMachine) {
        throw ("Expected QEMU PE machine 0x{0:X4}, found 0x{1:X4}" -f $expectedMachine, $qemuMachine)
    }
    $inventory = @(
        "runtime version: $($manifest.runtime_version)",
        "target platform: $($runtimeInfo.target_platform)",
        ("PE machine: 0x{0:X4}" -f $qemuMachine),
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
    Write-ProgressLine "bundled runtime inventory and QEMU PE architecture verified"

    if ($ProbeQemuStartupOnly) {
        Write-ProgressLine "probing installed QEMU startup contexts and serial backends without starting the desktop or guest"
        $probeResults = Invoke-QemuStartupProbe $qemuExe $versionRoot $EvidenceDirectory $ExpectedRuntimePlatform $env:MSYS2_ROOT
        foreach ($probeResult in $probeResults) {
            Write-ProgressLine "QEMU probe=$($probeResult.ProbeName): $($probeResult.Status)"
        }
        $probeUninstallers = @(Get-ChildItem -LiteralPath $installRoot -Filter "unins*.exe" -File -Recurse -ErrorAction SilentlyContinue)
        if ($probeUninstallers.Count -ne 1) {
            throw "Expected one NSIS uninstaller after QEMU startup probes; found $($probeUninstallers.Count)"
        }
        $probeUninstall = Start-Process -FilePath $probeUninstallers[0].FullName -ArgumentList "/S" -Wait -PassThru
        if ($probeUninstall.ExitCode -ne 0) { throw "NSIS uninstaller exited with $($probeUninstall.ExitCode)" }
        return
    }

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
    [DllImport("user32.dll", SetLastError = true)]
    public static extern bool PostMessage(IntPtr window, uint message, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool AttachThreadInput(uint attach, uint attachTo, bool attachInput);
    public delegate bool EnumWindowsProc(IntPtr window, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr window);

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

    public static IntPtr FindMainVisibleWindow(uint processId) {
        IntPtr bestWindow = IntPtr.Zero;
        long bestArea = 0;
        EnumWindows(delegate(IntPtr window, IntPtr lParam) {
            uint candidateProcessId;
            GetWindowThreadProcessId(window, out candidateProcessId);
            if (candidateProcessId != processId || !IsWindowVisible(window)) return true;
            StringBuilder className = new StringBuilder(256);
            GetClassName(window, className, className.Capacity);
            if (className.ToString() == "ConsoleWindowClass") return true;
            RECT rect;
            if (!GetClientRect(window, out rect)) return true;
            long width = Math.Max(0, rect.Right - rect.Left);
            long height = Math.Max(0, rect.Bottom - rect.Top);
            long area = width * height;
            if (area > bestArea) {
                bestArea = area;
                bestWindow = window;
            }
            return true;
        }, IntPtr.Zero);
        return bestWindow;
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
        // The empty-library header sits in the upper-middle of the content pane.
        // Scale its vertical position with the maximized client area instead of
        // assuming the old toolbar layout at the top of the window.
        POINT point = new POINT(client.Right - 65, client.Bottom * 355 / 1000);
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
    function Dismiss-AmbientWindowsShellPrompt([IntPtr]$WindowHandle) {
        if ($WindowHandle -eq [IntPtr]::Zero) { return $false }
        $description = [RootglassSmokeWindow]::DescribeWindow($WindowHandle)
        $accountPrompt = $description -match "title='(?:Microsoft account|Search)' class='Windows.UI.Core.CoreWindow'"
        $wslShellWindow = $description -match "title='C:\\Windows\\system32\\wsl\.exe' class='CASCADIA_HOSTING_WINDOW_CLASS'"
        if (-not $accountPrompt -and -not $wslShellWindow) {
            return $false
        }
        if ([RootglassSmokeWindow]::GetForegroundWindow() -ne $WindowHandle) {
            return $false
        }
        Write-ProgressLine "Windows runner surfaced an ambient account/search/WSL window; dismissing without input"
        $closePosted = [RootglassSmokeWindow]::PostMessage(
            $WindowHandle,
            [uint32]0x0010,
            [IntPtr]::Zero,
            [IntPtr]::Zero
        )
        Write-ProgressLine "posted WM_CLOSE to the foreground Windows shell prompt: $closePosted"
        Start-Sleep -Milliseconds 500
        if ([RootglassSmokeWindow]::GetForegroundWindow() -ne $WindowHandle) {
            Write-ProgressLine "dismissed the ambient Windows shell prompt with WM_CLOSE"
            return $true
        }
        [System.Windows.Forms.SendKeys]::SendWait("{ESC}")
        Start-Sleep -Milliseconds 500
        if ([RootglassSmokeWindow]::GetForegroundWindow() -eq $WindowHandle) {
            [System.Windows.Forms.SendKeys]::SendWait("%{F4}")
            Start-Sleep -Milliseconds 500
        }
        if ([RootglassSmokeWindow]::GetForegroundWindow() -eq $WindowHandle) {
            throw "Could not dismiss the ambient Windows shell prompt without credentials: $description"
        }
        Write-ProgressLine "dismissed the ambient Windows shell prompt without entering credentials"
        return $true
    }
    Write-ProgressLine "opening the example through the GPUI file picker"
    Complete-WindowsPrivacyOobe
    $RootglassProcess = Start-Process -FilePath $RootglassExe -PassThru
    $windowDeadline = (Get-Date).AddSeconds(30)
    $RootglassWindowHandle = [IntPtr]::Zero
    while ((Get-Date) -lt $windowDeadline) {
        $RootglassProcess.Refresh()
        $RootglassWindowHandle = [RootglassSmokeWindow]::FindMainVisibleWindow([uint32]$RootglassProcess.Id)
        if ($RootglassWindowHandle -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($RootglassWindowHandle -eq [IntPtr]::Zero) {
        throw "Installed Rootglass did not expose a visible non-console window for the picker smoke. MainWindowHandle: $([RootglassSmokeWindow]::DescribeWindow($RootglassProcess.MainWindowHandle))"
    }
    Write-ProgressLine "selected Rootglass UI window $([RootglassSmokeWindow]::DescribeWindow($RootglassWindowHandle)); process main window $([RootglassSmokeWindow]::DescribeWindow($RootglassProcess.MainWindowHandle))"
    $foregroundWindow = [IntPtr]::Zero
    $foregroundSet = $false
    for ($focusAttempt = 0; $focusAttempt -lt 3; $focusAttempt++) {
        $foregroundSet = [RootglassSmokeWindow]::ActivateWindow($RootglassWindowHandle)
        Start-Sleep -Milliseconds 500
        $foregroundWindow = [RootglassSmokeWindow]::GetForegroundWindow()
        if ($foregroundWindow -eq $RootglassWindowHandle) { break }
        if (-not (Dismiss-AmbientWindowsShellPrompt $foregroundWindow)) { break }
    }
    Write-ProgressLine "foreground request=$foregroundSet target=$RootglassWindowHandle active=$foregroundWindow"
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
    if ($foregroundWindow -ne $RootglassWindowHandle) {
        throw "Could not bring installed Rootglass to the foreground. $([RootglassSmokeWindow]::DescribeWindow($foregroundWindow))"
    }
    $importMethod = ""
    $importBounds = $null
    try {
        $windowAutomation = [System.Windows.Automation.AutomationElement]::FromHandle($RootglassWindowHandle)
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
            $clickPoint = [RootglassSmokeWindow]::ClickImportGlass($RootglassWindowHandle)
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
                        $windowHandle -eq $RootglassWindowHandle) { continue }

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
        if ($activeDialogDescription -match "class='CASCADIA_HOSTING_WINDOW_CLASS'" -and
            (Dismiss-AmbientWindowsShellPrompt $activeDialogHandle)) {
            Start-Sleep -Milliseconds 750
            $activeDialogHandle = [RootglassSmokeWindow]::GetForegroundWindow()
            $activeDialogDescription = [RootglassSmokeWindow]::DescribeWindow($activeDialogHandle)
            Write-ProgressLine "foreground window after dismissing WSL: $activeDialogDescription"
        }
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
        ("Rootglass PE machine: 0x{0:X4}" -f $rootglassMachine),
        ("QEMU PE machine: 0x{0:X4}" -f $qemuMachine),
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
    $tuiOutputLog = Join-Path $EvidenceDirectory "tui-smoke-output.txt"
    $tuiErrorLog = Join-Path $EvidenceDirectory "tui-smoke-error.txt"
    $processMonitor = Join-Path $EvidenceDirectory "qemu-process-monitor.txt"
    $processDetails = Join-Path $EvidenceDirectory "qemu-process-details.txt"
    $sampledQemuProcesses = @{}
    $pythonArguments = @("-B", $TuiSmokeScript, $RootglassExe, $glass[0].FullName, $tuiOutput)
    $argumentLine = ($pythonArguments | ForEach-Object { '"{0}"' -f $_.Replace('"', '\"') }) -join " "
    $repositoryRoot = Split-Path -Parent $PSScriptRoot
    $tuiProcess = Start-Process -FilePath $python -ArgumentList $argumentLine `
        -WorkingDirectory $repositoryRoot -PassThru -NoNewWindow `
        -RedirectStandardOutput $tuiOutputLog -RedirectStandardError $tuiErrorLog
    "timestamp pid cpu_seconds working_set_bytes thread_count thread_wait_states tcp_listeners" |
        Set-Content -LiteralPath $processMonitor -Encoding utf8
    while (-not $tuiProcess.HasExited) {
        foreach ($qemu in @(Get-RootglassQemuProcesses $qemuExe)) {
            $process = Get-Process -Id $qemu.ProcessId -ErrorAction SilentlyContinue
            $threadWaitStates = @(Get-CimInstance Win32_Thread `
                -Filter "ProcessHandle = '$($qemu.ProcessId)'" -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.ThreadState):$($_.ThreadWaitReason)" })
            $listeners = @(Get-NetTCPConnection -OwningProcess $qemu.ProcessId -State Listen -ErrorAction SilentlyContinue |
                ForEach-Object { "$($_.LocalAddress):$($_.LocalPort)" })
            $cpu = if ($process) { $process.CPU } else { "unknown" }
            $workingSet = if ($process) { $process.WorkingSet64 } else { "unknown" }
            $threads = if ($process) { $process.Threads.Count } else { "unknown" }
            $waitText = if ($threadWaitStates.Count -gt 0) { $threadWaitStates -join ";" } else { "unknown" }
            $listenerText = if ($listeners.Count -gt 0) { $listeners -join ";" } else { "none" }
            "$(Get-Date -Format o) $($qemu.ProcessId) $cpu $workingSet $threads $waitText $listenerText" |
                Add-Content -LiteralPath $processMonitor -Encoding utf8
            if (-not $sampledQemuProcesses.ContainsKey($qemu.ProcessId)) {
                $sampledQemuProcesses[$qemu.ProcessId] = $true
                try {
                    $moduleNames = @($process.Modules | ForEach-Object { $_.ModuleName }) -join ";"
                } catch {
                    $moduleNames = "unavailable: $($_.Exception.Message)"
                }
                @(
                    "ProcessId: $($qemu.ProcessId)",
                    "ExecutablePath: $($qemu.ExecutablePath)",
                    "CommandLine: $($qemu.CommandLine)",
                    "LoadedModules: $moduleNames",
                    "InitialThreadWaitStates: $waitText",
                    "InitialTcpListeners: $listenerText",
                    ""
                ) | Add-Content -LiteralPath $processDetails -Encoding utf8
            }
        }
        Start-Sleep -Seconds 1
        $tuiProcess.Refresh()
    }
    $tuiProcess.WaitForExit()
    if ($tuiProcess.ExitCode -ne 0) {
        Get-Content -LiteralPath $tuiOutputLog, $tuiErrorLog -ErrorAction SilentlyContinue
        throw "Installed TUI smoke exited with $($tuiProcess.ExitCode)"
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
