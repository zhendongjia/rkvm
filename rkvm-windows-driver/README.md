# rkvm Windows virtual HID driver

This optional KMDF driver gives the Windows client a real virtual keyboard and
mouse path. The client prefers `\\.\RkvmVirtualHid` when present and otherwise
falls back to `SendInput`. VHF reports are accepted only from LocalSystem and
administrators.

The build is pinned to Microsoft's WDK NuGet package in `packages.config`.
Restore it with `nuget restore -PackagesDirectory packages`, then build
`rkvmvhid.vcxproj` with the 64-bit MSBuild executable (`Bin/amd64/MSBuild.exe`)
for `Release|x64`; the pinned WDK includes 64-bit INF verification tools.

The driver also exposes Consumer Control volume-up, volume-down, and mute
buttons. Upgrade the client and driver together for these controls: the older
driver accepts only keyboard and mouse reports. Held volume keys repeat; mute
does not. The Power key is exposed as a standard System Control power button,
without auto-repeat, and follows Windows' configured power-button action. It
requires the virtual HID driver; the SendInput fallback logs and ignores it.
Closing the client handle releases all four report collections.

Development builds are test drivers. Installing one requires Windows test
signing and a reboot; do not change Secure Boot, BitLocker, or boot settings
without an explicit decision by the machine owner. A generally distributed
package must use Microsoft's production driver-signing process.

After the owner has disabled Secure Boot, enabled `TESTSIGNING`, and rebooted,
run `install-test-driver.ps1` from an elevated PowerShell. The script refuses
to proceed unless both preconditions are already true. It trusts the build's
test certificate, installs the root-enumerated device, verifies that its
write-only control handle opens, and restarts `rkvm-client` when present.

`uninstall-test-driver.ps1` removes the device and test certificate. It does
not change boot policy; disable `TESTSIGNING`, reboot, and re-enable Secure Boot
separately after testing.
