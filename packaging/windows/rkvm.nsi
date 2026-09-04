Unicode true
RequestExecutionLevel admin
ManifestDPIAware true
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "nsDialogs.nsh"
!include "Sections.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
!error "VERSION must be provided by build-installer.sh"
!endif
!ifndef PRODUCT_VERSION
!error "PRODUCT_VERSION must be provided by build-installer.sh"
!endif
!ifndef CLIENT_EXE
!error "CLIENT_EXE must be provided by build-installer.sh"
!endif
!ifndef CERTIFICATE_FILE
!error "CERTIFICATE_FILE must be provided by build-installer.sh"
!endif
!ifndef DRIVER_ROOT
!error "DRIVER_ROOT must be provided by build-installer.sh"
!endif
!ifndef DEVCON_EXE
!error "DEVCON_EXE must be provided by build-installer.sh"
!endif
!ifndef OUTPUT_EXE
!error "OUTPUT_EXE must be provided by build-installer.sh"
!endif
!ifndef DEFAULT_SERVER
!define DEFAULT_SERVER "127.0.0.1:5258"
!endif

!define PRODUCT_NAME "rkvm Windows client"
!define COMPANY_NAME "rkvm contributors"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\rkvm-client"

Name "${PRODUCT_NAME} ${VERSION}"
OutFile "${OUTPUT_EXE}"
InstallDir "$PROGRAMFILES64\rkvm"
InstallDirRegKey HKLM "Software\rkvm" "InstallDirectory"
BrandingText "rkvm Windows client"
VIProductVersion "${PRODUCT_VERSION}"
VIAddVersionKey /LANG=1033 "ProductName" "${PRODUCT_NAME}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "CompanyName" "${COMPANY_NAME}"
VIAddVersionKey /LANG=1033 "FileDescription" "rkvm Windows client installer"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright rkvm contributors"

Var ServerInput
Var PasswordInput
Var ServerValue
Var PasswordValue

!define MUI_ABORTWARNING
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${LICENSE_FILE}"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
Page custom ConfigPageCreate ConfigPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function ConfigPageCreate
    !insertmacro MUI_HEADER_TEXT "Client configuration" \
        "Enter the server endpoint and shared password."
    nsDialogs::Create 1018
    Pop $0
    ${If} $0 == error
        Abort
    ${EndIf}

    ${NSD_CreateLabel} 0 0 100% 12u "rkvm server (hostname:port, IPv4:port, or [IPv6]:port):"
    Pop $0
    ${NSD_CreateText} 0 16u 100% 13u "$ServerValue"
    Pop $ServerInput

    ${NSD_CreateLabel} 0 42u 100% 12u "Shared password (stored under a restricted ACL):"
    Pop $0
    ${NSD_CreatePassword} 0 58u 100% 13u "$PasswordValue"
    Pop $PasswordInput

    ${NSD_CreateLabel} 0 84u 100% 42u \
        "The server public certificate is embedded in this installer. The optional virtual HID component requires Secure Boot off and TESTSIGNING active before installation."
    Pop $0

    nsDialogs::Show
FunctionEnd

Function ConfigPageLeave
    ${NSD_GetText} $ServerInput $ServerValue
    ${NSD_GetText} $PasswordInput $PasswordValue
    ${If} $ServerValue == ""
        MessageBox MB_ICONSTOP "Enter an rkvm server endpoint."
        Abort
    ${EndIf}
    ${If} $PasswordValue == ""
        MessageBox MB_ICONSTOP "Enter the shared password used by rkvm-server."
        Abort
    ${EndIf}
FunctionEnd

Section "Client service (required)" SecClient
    SectionIn RO
    SetShellVarContext all
    SetRegView 64

    SetOutPath "$INSTDIR\.staging"
    File /oname=rkvm-client.exe "${CLIENT_EXE}"
    File /oname=certificate.pem "${CERTIFICATE_FILE}"

    SetOutPath "$INSTDIR"
    File /oname=README-Windows.txt "${README_FILE}"
    File /oname=install-client.ps1 "${INSTALL_CLIENT_SCRIPT}"

    System::Call 'Kernel32::SetEnvironmentVariableW(w "RKVM_SETUP_PASSWORD", w "$PasswordValue") i.r0'
    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -InputFormat None -ExecutionPolicy Bypass -File "$INSTDIR\install-client.ps1" -InstallDirectory "$INSTDIR" -ClientSource "$INSTDIR\.staging\rkvm-client.exe" -Server "$ServerValue" -CertificateSource "$INSTDIR\.staging\certificate.pem"' $0
    System::Call 'Kernel32::SetEnvironmentVariableW(w "RKVM_SETUP_PASSWORD", p 0) i.r1'
    ${If} $0 != 0
        MessageBox MB_ICONSTOP "Could not install the rkvm client service (exit code $0)."
        Abort
    ${EndIf}
    CopyFiles /SILENT "$INSTDIR\.staging\certificate.pem" "$INSTDIR\certificate.pem"
    RMDir /r "$INSTDIR\.staging"
    Delete "$INSTDIR\install-client.ps1"

    WriteUninstaller "$INSTDIR\uninstall.exe"
    WriteRegStr HKLM "Software\rkvm" "InstallDirectory" "$INSTDIR"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${PRODUCT_NAME}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${COMPANY_NAME}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\rkvm-client.exe"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

Section /o "Test-signed virtual HID driver" SecDriver
    SetOutPath "$INSTDIR\driver"
    File /oname=install-test-driver.ps1 "${DRIVER_ROOT}\install-test-driver.ps1"
    File /oname=uninstall-test-driver.ps1 "${DRIVER_ROOT}\uninstall-test-driver.ps1"

    SetOutPath "$INSTDIR\driver\x64\Release"
    File /oname=rkvmvhid.cer "${DRIVER_ROOT}\x64\Release\rkvmvhid.cer"
    SetOutPath "$INSTDIR\driver\x64\Release\rkvmvhid"
    File /oname=rkvmvhid.inf "${DRIVER_ROOT}\x64\Release\rkvmvhid\rkvmvhid.inf"
    File /oname=rkvmvhid.cat "${DRIVER_ROOT}\x64\Release\rkvmvhid\rkvmvhid.cat"
    File /oname=rkvmvhid.sys "${DRIVER_ROOT}\x64\Release\rkvmvhid\rkvmvhid.sys"
    SetOutPath "$INSTDIR\driver\packages\Microsoft.Windows.WDK.x64.10.0.28000.2526\c\tools\10.0.28000.0\x64"
    File /oname=devcon.exe "${DEVCON_EXE}"

    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -InputFormat None -ExecutionPolicy Bypass -File "$INSTDIR\driver\uninstall-test-driver.ps1"' $0
    ${If} $0 != 0
        MessageBox MB_ICONSTOP \
            "Could not remove the previous virtual HID installation (exit code $0)."
        Abort
    ${EndIf}

    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -InputFormat None -ExecutionPolicy Bypass -File "$INSTDIR\driver\install-test-driver.ps1"' $0
    ${If} $0 != 0
        MessageBox MB_ICONEXCLAMATION \
            "The client service was installed, but the virtual HID driver was not (exit code $0). Verify Secure Boot and TESTSIGNING, then rerun setup."
    ${Else}
        WriteRegDWORD HKLM "Software\rkvm" "VirtualHidInstalled" 1
    ${EndIf}
SectionEnd

Function .onInit
    ${IfNot} ${RunningX64}
        MessageBox MB_ICONSTOP "This package supports 64-bit Windows only."
        Abort
    ${EndIf}
    SetRegView 64
    SetShellVarContext all
    StrCpy $ServerValue "${DEFAULT_SERVER}"
    ReadEnvStr $PasswordValue "RKVM_SETUP_PASSWORD"
    ${GetParameters} $R0
    ClearErrors
    ${GetOptions} $R0 "/PASSWORD_FILE=" $R1
    ${IfNot} ${Errors}
        ClearErrors
        FileOpen $R2 "$R1" r
        ${If} ${Errors}
            MessageBox MB_ICONSTOP "Could not read the temporary password file."
            Abort
        ${EndIf}
        FileRead $R2 $PasswordValue
        FileClose $R2
    ${EndIf}

    ReadEnvStr $0 "RKVM_INSTALL_DRIVER"
    ${If} $0 == "1"
        !insertmacro SelectSection ${SecDriver}
    ${EndIf}
    ClearErrors
    ${GetOptions} $R0 "/WITHDRIVER=" $R1
    ${IfNot} ${Errors}
        ${If} $R1 == "1"
            !insertmacro SelectSection ${SecDriver}
        ${EndIf}
    ${EndIf}
FunctionEnd

LangString DESC_SecClient ${LANG_ENGLISH} \
    "Install the automatic LocalSystem rkvm client service and protected configuration."
LangString DESC_SecDriver ${LANG_ENGLISH} \
    "Install the optional test-signed virtual keyboard and mouse driver for sign-in, lock, and UAC desktops."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SecClient} $(DESC_SecClient)
    !insertmacro MUI_DESCRIPTION_TEXT ${SecDriver} $(DESC_SecDriver)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
    SetShellVarContext all
    SetRegView 64

    IfFileExists "$INSTDIR\driver\uninstall-test-driver.ps1" 0 driver_done
    ExecWait '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -InputFormat None -ExecutionPolicy Bypass -File "$INSTDIR\driver\uninstall-test-driver.ps1"' $0
    ${If} $0 != 0
        MessageBox MB_ICONSTOP \
            "Could not remove the virtual HID driver (exit code $0). The installed files were kept so removal can be retried."
        Abort
    ${EndIf}
driver_done:

    nsExec::ExecToLog '"$SYSDIR\sc.exe" stop rkvm-client'
    Pop $0
    nsExec::ExecToLog '"$SYSDIR\sc.exe" delete rkvm-client'
    Pop $0

    DeleteRegKey HKLM "${UNINSTALL_KEY}"
    DeleteRegKey HKLM "Software\rkvm"
    RMDir /r "$APPDATA\rkvm"
    RMDir /r "$INSTDIR\driver"
    Delete "$INSTDIR\rkvm-client.exe"
    Delete "$INSTDIR\certificate.pem"
    Delete "$INSTDIR\README-Windows.txt"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
SectionEnd
