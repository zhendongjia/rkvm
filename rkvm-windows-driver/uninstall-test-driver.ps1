[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}

$devcon = Join-Path $PSScriptRoot `
    'packages\Microsoft.Windows.WDK.x64.10.0.28000.2526\c\tools\10.0.28000.0\x64\devcon.exe'
if (-not (Test-Path -LiteralPath $devcon)) {
    throw "DevCon is missing: $devcon"
}

$service = Get-Service -Name 'rkvm-client' -ErrorAction SilentlyContinue
if ($service -and $service.Status -ne 'Stopped') {
    Stop-Service -Name 'rkvm-client'
}
try {
    & $devcon remove 'Root\RKVMVHID'
    if ($LASTEXITCODE -ne 0) { throw "Driver removal failed with exit code $LASTEXITCODE." }
} finally {
    if ($service) { Start-Service -Name 'rkvm-client' }
}

$certificate = Join-Path $PSScriptRoot 'x64\Release\rkvmvhid.cer'
if (Test-Path -LiteralPath $certificate) {
    $thumbprint = [Security.Cryptography.X509Certificates.X509Certificate2]::new($certificate).Thumbprint
    foreach ($storeName in @('Root', 'TrustedPublisher')) {
        $store = [Security.Cryptography.X509Certificates.X509Store]::new(
            $storeName,
            [Security.Cryptography.X509Certificates.StoreLocation]::LocalMachine)
        $store.Open([Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
        try {
            foreach ($match in @($store.Certificates.Find(
                [Security.Cryptography.X509Certificates.X509FindType]::FindByThumbprint,
                $thumbprint,
                $false))) {
                $store.Remove($match)
            }
        } finally {
            $store.Close()
        }
    }
}
Write-Host 'rkvm virtual HID device and its test certificate were removed.'
Write-Host 'This script intentionally does not change TESTSIGNING or Secure Boot.'
