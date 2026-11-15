param(
    [Parameter(Mandatory = $true)][string]$InputInstaller,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [Parameter(Mandatory = $true)][ValidateSet("x64", "arm64")][string]$Architecture
)

$ErrorActionPreference = "Stop"
foreach ($name in @("WINDOWS_CERTIFICATE_PFX_BASE64", "WINDOWS_CERTIFICATE_PASSWORD")) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        throw "Protected Windows signing secret $name is required"
    }
}

$inputPath = (Resolve-Path -LiteralPath $InputInstaller).Path
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$outputPath = Join-Path $OutputDirectory (Split-Path -Leaf $inputPath)
$pfxPath = Join-Path $env:RUNNER_TEMP "rootglass-release-signing.pfx"
$certificate = $null
try {
    [IO.File]::WriteAllBytes(
        $pfxPath,
        [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE_PFX_BASE64)
    )
    Copy-Item -LiteralPath $inputPath -Destination $outputPath -Force
    $password = ConvertTo-SecureString $env:WINDOWS_CERTIFICATE_PASSWORD -AsPlainText -Force
    $importedCertificates = @(Import-PfxCertificate -FilePath $pfxPath `
        -CertStoreLocation Cert:\CurrentUser\My -Password $password)
    $certificate = $importedCertificates |
        Where-Object { $_.HasPrivateKey } |
        Select-Object -First 1
    if (-not $certificate -or -not $certificate.HasPrivateKey) {
        throw "The imported Windows certificate has no private key"
    }

    $signTool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $signTool) {
        $windowsKits = Join-Path ${env:ProgramFiles(x86)} "Windows Kits/10/bin"
        $signTool = Get-ChildItem -Path $windowsKits -Filter signtool.exe -File -Recurse `
            -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "\\x64\\signtool\.exe$" } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
    }
    if (-not $signTool) { throw "Windows SDK signtool.exe was not found" }
    $signToolPath = if ($signTool.Path) { $signTool.Path } else { $signTool.FullName }

    & $signToolPath sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
        /sha1 $certificate.Thumbprint /s My $outputPath
    if ($LASTEXITCODE -ne 0) { throw "signtool failed to sign the $Architecture installer" }
    & $signToolPath verify /pa /all /v $outputPath
    if ($LASTEXITCODE -ne 0) { throw "Authenticode verification failed for $outputPath" }

    $signature = Get-AuthenticodeSignature -LiteralPath $outputPath
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid) {
        throw "Installer signature status is $($signature.Status), expected Valid"
    }
    $report = [ordered]@{
        schema_version = 1
        platform = "Windows $Architecture"
        installer = (Split-Path -Leaf $outputPath)
        signature_status = $signature.Status.ToString()
        certificate_subject = $signature.SignerCertificate.Subject
        certificate_thumbprint = $signature.SignerCertificate.Thumbprint
        timestamp_certificate_subject = if ($signature.TimeStamperCertificate) {
            $signature.TimeStamperCertificate.Subject
        } else { $null }
        timestamp_url = "http://timestamp.digicert.com"
        verification = "signtool verify /pa /all /v"
    }
    $reportPath = Join-Path $OutputDirectory "SIGNING-PROVENANCE-windows-$Architecture.json"
    $report | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 $reportPath
}
finally {
    if ($certificate) {
        Remove-Item -LiteralPath "Cert:\CurrentUser\My\$($certificate.Thumbprint)" `
            -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -LiteralPath $pfxPath -Force -ErrorAction SilentlyContinue
}
