rkvm Windows installer
======================

The installer adds rkvm-client as an automatic LocalSystem service. It embeds
the server's public TLS certificate, but never embeds the shared password; enter
the same password used by rkvm-server during setup.

The optional virtual HID driver is development/test signed. To install it,
Secure Boot must already be disabled and Windows TESTSIGNING mode must already
be active. The installer deliberately does not change either boot-security
setting. Without this driver, input works on an unlocked desktop but Windows can
reject it at the sign-in, lock, and UAC secure desktops.

This installer and its driver are not production code-signed. Windows may show
an Unknown publisher warning. Uninstalling removes the service, driver test
certificate, installed files, and C:\ProgramData\rkvm (including its password).
It does not re-enable Secure Boot or disable TESTSIGNING mode.
