[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script from an elevated PowerShell window.'
}

$secureBoot = try { Confirm-SecureBootUEFI } catch { $null }
if ($secureBoot -eq $true) {
    throw 'Secure Boot is enabled. The development-signed driver cannot be loaded.'
}

$startOptions = Get-ItemPropertyValue `
    -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control' `
    -Name SystemStartOptions
if (@($startOptions -split '\s+') -notcontains 'TESTSIGNING') {
    throw 'Windows test-signing mode is not active for the current boot.'
}

$package = Join-Path $PSScriptRoot 'x64\Release\rkvmvhid'
$inf = Join-Path $package 'rkvmvhid.inf'
$certificate = Join-Path $PSScriptRoot 'x64\Release\rkvmvhid.cer'
$devcon = Join-Path $PSScriptRoot `
    'packages\Microsoft.Windows.WDK.x64.10.0.28000.2526\c\tools\10.0.28000.0\x64\devcon.exe'
foreach ($path in @($inf, $certificate, $devcon)) {
    if (-not (Test-Path -LiteralPath $path)) {
        throw "Required build output is missing: $path"
    }
}

& "$env:SystemRoot\System32\certutil.exe" -addstore -f Root $certificate | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not trust the test certificate as a root.' }
& "$env:SystemRoot\System32\certutil.exe" -addstore -f TrustedPublisher $certificate | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not trust the test certificate as a publisher.' }

$clientService = Get-Service -Name 'rkvm-client' -ErrorAction SilentlyContinue
$restartClient = $clientService -and
    $clientService.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped
if ($restartClient) {
    Stop-Service -Name 'rkvm-client'
    $clientService.WaitForStatus(
        [System.ServiceProcess.ServiceControllerStatus]::Stopped,
        [TimeSpan]::FromSeconds(10))
}

try {
    $existingDevice = @(& $devcon find 'Root\RKVMVHID' 2>&1) -match `
        '(?i)^ROOT\\[^:]+\s*:'
    if ($existingDevice) {
        & $devcon update $inf 'Root\RKVMVHID'
    } else {
        & $devcon install $inf 'Root\RKVMVHID'
    }
    $devconExitCode = $LASTEXITCODE
    if ($devconExitCode -notin @(0, 1)) {
        throw "Driver installation failed with exit code $devconExitCode."
    }
    if ($devconExitCode -eq 1) {
        Write-Warning 'DevCon reports that Windows must restart to finish the driver update.'
    }

    if (-not ('Rkvm.VirtualHidNative' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Rkvm {
    public static class VirtualHidNative {
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern SafeFileHandle CreateFile(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            IntPtr securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);
    }
}
'@
    }
    $handle = [Rkvm.VirtualHidNative]::CreateFile(
        '\\.\RkvmVirtualHid',
        0x40000000,
        0,
        [IntPtr]::Zero,
        3,
        0,
        [IntPtr]::Zero)
    if ($handle.IsInvalid) {
        $win32Error = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
        $handle.Dispose()
        throw "Could not open the rkvm virtual HID device (Win32 error $win32Error)."
    }
    $handle.Dispose()
}
finally {
    if ($restartClient) {
        Start-Service -Name 'rkvm-client'
    }
}
Write-Host 'rkvm virtual HID driver installed and opened successfully.'
