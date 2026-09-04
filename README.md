# rkvm
[![rkvm](https://img.shields.io/aur/version/rkvm)](https://aur.archlinux.org/packages/rkvm)

rkvm is a tool for sharing keyboard and mouse across Linux machines and
Windows clients.
It is based on a client/server architecture, where server is the machine controlling mouse and keyboard and relays events (mouse move, key presses, ...) to clients.

Switching between different clients is done by a configurable keyboard shortcut.

## Features
- TLS encrypted by default, backed by [rustls](https://github.com/rustls/rustls)
- Display server agnostic (in fact, it doesn't require a display server at all)
- Low overhead

## Requirements
- Linux server: the uinput Linux kernel module, enabled by default in most distros. You can confirm that it's enabled in your distro by checking that `/dev/uinput` exists.
- Linux server: libevdev development files (`sudo apt install libevdev-dev` on Debian/Ubuntu)
- Linux server: Clang/LLVM (`sudo apt install clang` on Debian/Ubuntu)
- Windows client: Windows 10 or later. An unlocked desktop works through
  `SendInput` without a driver. Reliable input before logon and on the locked
  or UAC secure desktop requires the optional virtual HID driver in
  `rkvm-windows-driver`.

## Manual installation
If you can, it is strongly recommended to use the [AUR package](https://aur.archlinux.org/packages/rkvm) to install rkvm.  
Note that the master branch can contain untested and breaking changes - for regular use, it is recommended to pick the latest [release](https://github.com/htrefil/rkvm/releases) instead.

```
$ cargo build --release
# cp target/release/rkvm-client /usr/bin/
# cp target/release/rkvm-server /usr/bin/
# cp target/release/rkvm-certificate-gen /usr/bin/ # Optional
# cp systemd/rkvm-client.service /usr/lib/systemd/system/
# cp systemd/rkvm-server.service /usr/lib/systemd/system/
```

On Windows, build or download `rkvm-client.exe` and run it with the client
configuration file:

```
cargo build --release --package rkvm-client
target\\release\\rkvm-client.exe C:\\path\\to\\client.toml
```

The Windows port is a client only. The Linux machine remains the server and
continues to capture the physical keyboard and mouse. `rkvm-server` remains
Linux-only.

To receive input before a user logs on, install the client with an elevated
shell as an automatically started LocalSystem service. Put the executable in
an administrator-only writable directory such as `C:\\Program Files\\rkvm`,
and keep the client configuration, shared password, and server certificate in
an ACL-protected directory such as `C:\\ProgramData\\rkvm`:

```powershell
$binary = '"C:\Program Files\rkvm\rkvm-client.exe" --service "C:\ProgramData\rkvm\client.toml"'
New-Service -Name rkvm-client -BinaryPathName $binary -StartupType Automatic
Start-Service rkvm-client
```

The service starts a LocalSystem desktop agent in the active console session.
It restarts the agent on network failure and can move it between the normal and
logon desktops on logon or logoff. Locking and unlocking deliberately keeps the
same agent so its authenticated network connection is not interrupted. Service
mode therefore does not depend on a user account's startup programs. When the
optional virtual HID driver is installed, the agent uses it in preference to
`SendInput`, and its kernel input path continues to work while the agent stays
on the normal desktop during a lock. Without the driver, current Windows
releases reject input on the secure desktop even from a LocalSystem/UIAccess
agent. Because both the service and driver grant broad input access, keep their
installed files writable only by administrators.

Windows keyboard repeat follows the server's per-device delay and period.
Missing values default to a 500 ms delay and 33 ms period; an explicit zero
disables repeat. Modifier keys, lock keys, and mouse buttons do not repeat.
Held remote keys and buttons are released when a device is removed, a connection
fails or times out, or the client exits normally (including Ctrl+C).
The server also releases held inputs on the old destination when switching
machines, so a held key cannot keep repeating on a still-connected old client.

## Configuration
After installation:
- Generate a certificate and private key using the `rkvm-certificate-gen` tool or provide your own from other sources.
- For server, place both the certificate and private key in `/etc/rkvm/certificate.pem` and `/etc/rkvm/key.pem` respectively.
- For client, place the certificate to `/etc/rkvm/certificate.pem`.
- Create a config if you haven't done so already.  
  Server:  
  ```
  # cp /usr/share/rkvm/examples/server.toml /etc/rkvm/server.toml
  ```
  Client:
  ```
  # cp /usr/share/rkvm/examples/client.toml /etc/rkvm/client.toml
  ```
  Do not edit the example configs, they will be overwritten by your package manager.
- **Change the password** and optionally reconfigure the network listen address and key bindings for switching clients  
- Since rkvm-server grabs all input, i's a good idea to do a test run first to make sure you won't end up
  being unable to user your keyboard and/or mouse because your display server is not properly configured to receive input from rkvm.

  Run the following command to start rkvm-server for 15 seconds to test that your keyboard, mouse, etc. works properly:
  ```
  # rkvm-server /etc/rkvm/server.toml --shutdown-after 15
  ```

- Enable and start the systemd service.  
  Server:
  ```
  # systemctl enable rkvm-server
  # systemctl start rkvm-server
  ```
  Client:
  ```
  # systemctl enable rkvm-client
  # systemctl start rkvm-client
  ```

## Why rkvm and not Barrier/Synergy?
The author of this program had a lot of problems with said programs, namely his keyboard layout (Czech) not being supported properly, which stems from the fact that the programs send characters which it then attempts to translate back into keycodes. rkvm takes a different approach to solving this problem and doesn't assume anything about your keyboard layout -- it sends raw keycodes only.

Additionally, rkvm doesn't even know or care about X, Wayland or any display server that might be in use, because it uses the uinput API with libevdev to read and generate input events.

The Windows client uses the same raw-key event protocol as the Linux client.
It prefers full-state keyboard and mouse reports through the optional virtual
HID driver, and falls back to the Windows input API when the driver is absent.
Absolute devices, touch input, and some special or multimedia keys are not
currently represented by the Windows backend.

## Limitations
- The server and physical input capture remain Linux-only.
- Windows is currently supported as a client only. Direct command-line launches affect only their interactive desktop. Run it as a LocalSystem service and install the virtual HID driver for pre-logon and locked-console input.
- `SendInput` cannot drive the locked/secure desktop, and it cannot generate `Ctrl+Alt+Delete`. Secure-attention-sequence handling through the virtual HID driver has not yet been validated.
- Absolute devices, touch input, and some special or multimedia keys are not currently supported on Windows.
- Input cleanup is best effort after power loss. The virtual HID driver sends
  neutral keyboard and mouse reports when its client handle closes. Without
  the driver, Windows may reject releases because of desktop or integrity-level
  restrictions; see [SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput).

## Project structure
- `rkvm-server` - server application code
- `rkvm-client` - client application code
- `rkvm-input` - handles reading from and writing to input devices
- `rkvm-net` - network protocol encoding and decoding
- `rkvm-certificate-gen` - certificate generation tool
- `rkvm-windows-driver` - optional VHF virtual keyboard/mouse driver

[Bincode](https://github.com/servo/bincode) is used for encoding of messages on the network and [Tokio](https://tokio.rs) as an asynchronous runtime.

## Contributions
All contributions, that includes both PRs and issues, are very welcome.

## Donations
If you find rkvm useful, you can donate to the original author and maintainer using [Ko-fi](https://ko-fi.com/htrefil).

## License
[MIT](LICENSE)
