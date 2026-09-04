#Requires -Version 5.1
#Requires -RunAsAdministrator

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallDirectory,
    [Parameter(Mandatory = $true)]
    [string]$ClientSource,
    [Parameter(Mandatory = $true)]
    [string]$Server,
    [Parameter(Mandatory = $true)]
    [string]$CertificateSource
)

$ErrorActionPreference = 'Stop'
$serviceName = 'rkvm-client'
$dataDirectory = Join-Path $env:ProgramData 'rkvm'
$executable = Join-Path $InstallDirectory 'rkvm-client.exe'
$certificate = Join-Path $dataDirectory 'certificate.pem'
$config = Join-Path $dataDirectory 'client.toml'
$password = $env:RKVM_SETUP_PASSWORD

if (-not $password) {
    throw 'The shared password was not provided by the installer.'
}
if ($password.Length -gt 4096) {
    throw 'The shared password is too long.'
}
if ($Server -notmatch '^(?:\[[0-9A-Fa-f:]+\]|[^:\s]+):([0-9]{1,5})$') {
    throw 'The server must use hostname:port, IPv4:port, or [IPv6]:port syntax.'
}
$port = [int]$Matches[1]
if ($port -lt 1 -or $port -gt 65535) {
    throw 'The server port must be between 1 and 65535.'
}
foreach ($required in @($ClientSource, $CertificateSource)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Required installer file is missing: $required"
    }
}

function ConvertTo-TomlString([string]$Value) {
    $escaped = $Value.Replace('\', '\\').Replace('"', '\"')
    $escaped = $escaped.Replace("`r", '\r').Replace("`n", '\n')
    return '"' + $escaped + '"'
}

function Invoke-ServiceControl {
    param([Parameter(Mandatory = $true)][string[]]$ArgumentList)

    & "$env:SystemRoot\System32\sc.exe" @ArgumentList | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "sc.exe $($ArgumentList[0]) failed with exit code $LASTEXITCODE"
    }
}

function Set-PrivateAcl {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$AllowUsersRead
    )

    $rules = @(
        '/inheritance:r',
        '/grant:r',
        '*S-1-5-18:(OI)(CI)F',
        '*S-1-5-32-544:(OI)(CI)F'
    )
    if ($AllowUsersRead) {
        $rules += '*S-1-5-32-545:(OI)(CI)RX'
    }
    & "$env:SystemRoot\System32\icacls.exe" $Path @rules | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Could not secure $Path"
    }
    & "$env:SystemRoot\System32\icacls.exe" $Path '/setowner' '*S-1-5-32-544' | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Could not set the owner of $Path"
    }
}

$existing = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
if ($existing) {
    if ($existing.Status -ne [System.ServiceProcess.ServiceControllerStatus]::Stopped) {
        Stop-Service -Name $serviceName
        $existing.WaitForStatus(
            [System.ServiceProcess.ServiceControllerStatus]::Stopped,
            [TimeSpan]::FromSeconds(10))
    }
    Invoke-ServiceControl @('delete', $serviceName)
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Service -Name $serviceName -ErrorAction SilentlyContinue) -and
           (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
        throw 'The previous rkvm-client service is still pending deletion.'
    }
}

New-Item -ItemType Directory -Path $InstallDirectory -Force | Out-Null
New-Item -ItemType Directory -Path $dataDirectory -Force | Out-Null
Set-PrivateAcl -Path $InstallDirectory -AllowUsersRead
Set-PrivateAcl -Path $dataDirectory

Copy-Item -LiteralPath $ClientSource -Destination $executable -Force
Copy-Item -LiteralPath $CertificateSource -Destination $certificate -Force
$certificateForConfig = $certificate.Replace('\', '/')
$contents = @(
    "server = $(ConvertTo-TomlString $Server)",
    "certificate = $(ConvertTo-TomlString $certificateForConfig)",
    "password = $(ConvertTo-TomlString $password)"
) -join "`n"
[IO.File]::WriteAllText($config, $contents + "`n", [Text.UTF8Encoding]::new($false))

foreach ($logName in @('service.log', 'client-service.log')) {
    Remove-Item -LiteralPath (Join-Path $dataDirectory $logName) `
        -Force -ErrorAction SilentlyContinue
}

$binaryPath = "`"$executable`" --service `"$config`""
New-Service `
    -Name $serviceName `
    -BinaryPathName $binaryPath `
    -DisplayName 'rkvm Windows client' `
    -Description 'Receives keyboard and mouse input from an rkvm server.' `
    -StartupType Automatic `
    -DependsOn 'Tcpip' | Out-Null
Invoke-ServiceControl @(
    'failure', $serviceName,
    'reset=', '86400',
    'actions=', 'restart/5000/restart/10000/restart/30000')
Invoke-ServiceControl @('failureflag', $serviceName, '1')

Start-Service -Name $serviceName
$installed = Get-Service -Name $serviceName
$installed.WaitForStatus(
    [System.ServiceProcess.ServiceControllerStatus]::Running,
    [TimeSpan]::FromSeconds(15))
