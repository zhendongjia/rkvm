# rkvm Windows virtual HID driver

This optional KMDF driver gives the Windows client a real virtual keyboard and
mouse path. The client prefers `\\.\RkvmVirtualHid` when present and otherwise
falls back to `SendInput`. VHF reports are accepted only from LocalSystem and
administrators.

The build is pinned to Microsoft's WDK NuGet package in `packages.config`.
Restore it with `nuget restore -PackagesDirectory packages`, then build
`rkvmvhid.vcxproj` with MSBuild for `Release|x64`.

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
