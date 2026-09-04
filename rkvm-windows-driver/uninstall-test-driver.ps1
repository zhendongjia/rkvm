[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}

$service = Get-Service -Name 'rkvm-client' -ErrorAction SilentlyContinue
$restartService = $service -and $service.Status -ne 'Stopped'
if ($restartService) {
    Stop-Service -Name 'rkvm-client'
}
try {
    $pnputil = "$env:SystemRoot\System32\pnputil.exe"
    $driverDevices = @(Get-CimInstance Win32_PnPEntity | Where-Object {
        @($_.HardwareID) -contains 'Root\RKVMVHID'
    })
    if ($driverDevices.Count -gt 0) {
        & $pnputil /remove-device /deviceid 'Root\RKVMVHID' `
            /class System /subtree /force | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "Driver device-tree removal failed with exit code $LASTEXITCODE."
        }
        Start-Sleep -Milliseconds 500
    }

    $driverPackages = @(Get-WindowsDriver -Online -All | Where-Object {
        $_.OriginalFileName -match '(?i)[\\/]rkvmvhid\.inf$'
    })
    foreach ($driverPackage in $driverPackages) {
        & $pnputil /delete-driver `
            $driverPackage.Driver /uninstall /force | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "Could not delete driver package $($driverPackage.Driver)."
        }
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
} finally {
    if ($restartService) { Start-Service -Name 'rkvm-client' }
}
Write-Host 'rkvm virtual HID device and its test certificate were removed.'
Write-Host 'This script intentionally does not change TESTSIGNING or Secure Boot.'
