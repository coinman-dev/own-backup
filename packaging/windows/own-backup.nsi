; Installer for Own Backup.
;
; Built by scripts/build-installer.sh, which puts the files to pack next to a
; copy of this script and passes the version:
;   makensis -INPUTCHARSET UTF8 -DVERSION=0.1.1 -DVERSION_NUMERIC=0.1.1.0 own-backup.nsi
;
; Modelled on the installer of Own Keyboard Switch, with one difference that
; decides the rest: this program is a service that runs as LocalSystem. There
; is therefore no "only for me" variant and no folder an ordinary account may
; write to — a service executable in %LOCALAPPDATA%, or a Logs folder open to
; Users, would let any account choose what LocalSystem runs or where it writes.
; The installer asks for administrator rights once, in its manifest, and
; installs for the whole computer.
;
; RUSSIAN TEXT — the four things that go wrong, and what is done about each:
;
; 1. THIS FILE IS UTF-8 WITH A BYTE-ORDER MARK and has to stay so. makensis on
;    Linux reads a script as UTF-8 either way, but makensis.exe on Windows
;    reads one without the mark in the ANSI code page, and every Cyrillic
;    string below then compiles — without a warning — into «РџСЂРѕРіСЂР°РјРјР°».
;    The build script refuses a copy that lost the mark and passes
;    -INPUTCHARSET UTF8 as well.
; 2. `Unicode true`: an ANSI installer shows Russian correctly only on a
;    Windows whose system code page is 1251, and cannot install into a folder
;    whose name is outside that code page.
; 3. Nothing a child process prints is shown. taskkill answers in the OEM code
;    page (866) and our own executables in UTF-8; the details pane expects
;    neither. Progress is reported by the LangStrings below, and the outcome of
;    a command is its exit code.
; 4. Nothing is decided by a text Windows produced. Whether the service is
;    registered, and from where, is read from its registry key: `sc query`
;    answers «СОСТОЯНИЕ» on a Russian Windows and «STATE» on an English one.
;
; The elevated installer may be running under ANOTHER account (a standard user
; who typed an administrator's password), so HKCU here is not the person's who
; will use the program. Everything the installer remembers is in HKLM; the one
; per-user thing, the icon beside the clock, is registered by the window itself
; at its first start (gui/src/tray.rs).

Unicode true
ManifestDPIAware true
RequestExecutionLevel admin

!ifndef VERSION
    !error "VERSION is required; build through scripts/build-installer.sh"
!endif
!ifndef VERSION_NUMERIC
    !define VERSION_NUMERIC "0.0.0.0"
!endif
!ifndef OUTPUT
    !define OUTPUT "own-backup-install.exe"
!endif

!define NAME "Own Backup"
!define SHORT_NAME "OwnBackup"
!define PUBLISHER "coinman-dev"
!define GUI_EXE "own-backup-gui.exe"
!define SERVICE_EXE "own-backup-service.exe"
!define CLI_EXE "own-backup-cli.exe"
; The desktop of the boot medium. Not run on an installed system, but the
; service copies it from here when it builds a medium (backup/src/media_layout.rs).
!define SHELL_EXE "own-backup-shell.exe"
; The Total Commander plugin: files of Linux partitions, read through the
; service (crates/tc-plugin). Added in Total Commander by hand, from here.
!define TC_DIR "TotalCommander"
!define TC_PLUGIN "OwnBackup.wfx64"
; own_backup_common::WINDOWS_SERVICE_NAME
!define SERVICE_NAME "OwnBackup"
!define SERVICE_KEY "SYSTEM\CurrentControlSet\Services\${SERVICE_NAME}"
!define APP_KEY "Software\${SHORT_NAME}"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${SHORT_NAME}"
; The logon entry of the icon beside the clock, as gui/src/tray.rs writes it.
!define RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define RUN_VALUE "OwnBackupTray"
; Settings, jobs, history and remembered volume secrets. The same literal the
; program uses (common/src/config.rs, default_data_dir), not a shell folder:
; what is removed must be exactly what was written.
!define DATA_DIR "C:\ProgramData\OwnBackup"
; The identifier in gui/tauri.conf.json: the folder under %LOCALAPPDATA% where
; the window keeps what it remembers (theme, language) and its WebView cache.
!define WEBVIEW_DATA "com.ownbackup.gui"
; The WebView2 runtime, as gui/src/main.rs (webview2_registered) looks for it.
!define WEBVIEW2_CLIENT "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
!define WEBVIEW2_URL "https://go.microsoft.com/fwlink/p/?LinkId=2124703"

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "Sections.nsh"
!include "x64.nsh"

Name "${NAME}"
OutFile "${OUTPUT}"
; Empty until .onInit, so that a path found there can only have come from /D=.
InstallDir ""
BrandingText "${NAME} ${VERSION}"
ShowInstDetails show
ShowUninstDetails show
SetCompressor /SOLID lzma

; The version resource has one language block; its strings stay ASCII.
VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "Copyright (c) 2026 coinman-dev"

!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_FUNCTION RunProgram
!define MUI_COMPONENTSPAGE_SMALLDESC
!define MUI_LANGDLL_ALLLANGUAGES
; HKLM, not HKCU: the uninstaller has to find the language whoever starts it.
!define MUI_LANGDLL_REGISTRY_ROOT "HKLM"
!define MUI_LANGDLL_REGISTRY_KEY "${APP_KEY}"
!define MUI_LANGDLL_REGISTRY_VALUENAME "Language"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "LICENSE.txt"
!insertmacro MUI_PAGE_COMPONENTS
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipDirectoryOnUpgrade
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; Russian first, so it is the default of the language dialog.
!insertmacro MUI_LANGUAGE "Russian"
!insertmacro MUI_LANGUAGE "English"

; ----------------------------------------------------------------- strings

LangString DescMain ${LANG_RUSSIAN} "Программа, служба резервного копирования и командная строка. Обязательный компонент."
LangString DescMain ${LANG_ENGLISH} "The program, its backup service and the command line. Required."
LangString NameDesktop ${LANG_RUSSIAN} "Ярлык на рабочем столе"
LangString NameDesktop ${LANG_ENGLISH} "Desktop shortcut"
LangString DescDesktop ${LANG_RUSSIAN} "Создать ярлык на рабочем столе для всех пользователей."
LangString DescDesktop ${LANG_ENGLISH} "Create a shortcut on the desktop for all users."
LangString Need64 ${LANG_RUSSIAN} "Программа работает только на 64-разрядной Windows."
LangString Need64 ${LANG_ENGLISH} "The program only runs on 64-bit Windows."
LangString BadDir ${LANG_RUSSIAN} "Выберите папку для программы, а не корень диска."
LangString BadDir ${LANG_ENGLISH} "Choose a folder for the program, not a drive root."
LangString StoppingProgram ${LANG_RUSSIAN} "Закрытие окна программы и значка возле часов..."
LangString StoppingProgram ${LANG_ENGLISH} "Closing the program window and the icon beside the clock..."
LangString StoppingService ${LANG_RUSSIAN} "Остановка службы Own Backup. Если идёт резервное копирование, это может занять несколько минут..."
LangString StoppingService ${LANG_ENGLISH} "Stopping the Own Backup service. If a backup is running, this can take a few minutes..."
LangString ServiceStopFailed ${LANG_RUSSIAN} "Не удалось остановить службу Own Backup (код $0). Дождитесь окончания резервного копирования или остановите службу вручную и повторите."
LangString ServiceStopFailed ${LANG_ENGLISH} "The Own Backup service could not be stopped (code $0). Wait for the running backup to finish, or stop the service by hand, and try again."
LangString RegisteringService ${LANG_RUSSIAN} "Регистрация службы Own Backup..."
LangString RegisteringService ${LANG_ENGLISH} "Registering the Own Backup service..."
LangString ServiceInstallFailed ${LANG_RUSSIAN} "Не удалось зарегистрировать службу Own Backup (код $0)."
LangString ServiceInstallFailed ${LANG_ENGLISH} "The Own Backup service could not be registered (code $0)."
LangString StartingService ${LANG_RUSSIAN} "Запуск службы Own Backup..."
LangString StartingService ${LANG_ENGLISH} "Starting the Own Backup service..."
LangString ServiceStartFailed ${LANG_RUSSIAN} "Программа установлена, но служба Own Backup не запустилась (код $0).$\r$\n$\r$\nЖурнал службы:$\r$\n$INSTDIR\Logs"
LangString ServiceStartFailed ${LANG_ENGLISH} "The program is installed, but the Own Backup service did not start (code $0).$\r$\n$\r$\nThe service log:$\r$\n$INSTDIR\Logs"
LangString NoWebView2 ${LANG_RUSSIAN} "На этом компьютере не найден Microsoft Edge WebView2 Runtime. Без него окно Own Backup не откроется; служба и командная строка работают и так.$\r$\n$\r$\nОткрыть страницу загрузки Microsoft?"
LangString NoWebView2 ${LANG_ENGLISH} "Microsoft Edge WebView2 Runtime was not found on this computer. The Own Backup window will not open without it; the service and the command line work as they are.$\r$\n$\r$\nOpen the Microsoft download page?"
LangString RemovingService ${LANG_RUSSIAN} "Остановка и удаление службы Own Backup..."
LangString RemovingService ${LANG_ENGLISH} "Stopping and removing the Own Backup service..."
LangString ServiceRemoveFailed ${LANG_RUSSIAN} "Не удалось удалить службу Own Backup (код $0). Программа оставлена как есть; закройте оснастку «Службы», если она открыта, и повторите."
LangString ServiceRemoveFailed ${LANG_ENGLISH} "The Own Backup service could not be removed (code $0). The program was left as it is; close the Services console if it is open, and try again."
LangString ForeignService ${LANG_RUSSIAN} "Служба Own Backup зарегистрирована из другой папки и оставлена как есть:"
LangString ForeignService ${LANG_ENGLISH} "The Own Backup service is registered from another folder and was left alone:"
LangString AskRemoveData ${LANG_RUSSIAN} "Удалить также настройки, задания, историю и сохранённые ключи томов?$\r$\n${DATA_DIR}$\r$\n$\r$\nСами резервные копии не удаляются в любом случае. Если вы собираетесь установить программу заново, выберите «Нет»."
LangString AskRemoveData ${LANG_ENGLISH} "Also delete the settings, jobs, history and remembered volume keys?$\r$\n${DATA_DIR}$\r$\n$\r$\nThe backups themselves are never deleted. Choose No if you are going to install the program again."
LangString DataRemoveFailed ${LANG_RUSSIAN} "Настройки не удалось удалить полностью. Удалите эту папку вручную:"
LangString DataRemoveFailed ${LANG_ENGLISH} "The settings could not be deleted completely. Delete this folder by hand:"
LangString KeptData ${LANG_RUSSIAN} "Настройки оставлены в папке:"
LangString KeptData ${LANG_ENGLISH} "The settings were left in:"

; ---------------------------------------------------------------- helpers

; "yes" when installing over an earlier installation.
Var Upgrade
; "yes" when the uninstaller finds the service registered from another folder.
Var ForeignService

; The window and the icon beside the clock are the same executable, and both
; keep it open. A close request first, then force. taskkill answers 128 when
; there is nothing by that name, and then there is nothing to wait for.
!macro StopProgram UN
Function ${UN}StopProgram
    DetailPrint "$(StoppingProgram)"
    nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${GUI_EXE}"'
    Pop $0
    ${If} $0 == "128"
        Return
    ${EndIf}
    Sleep 1500
    nsExec::Exec '"$SYSDIR\taskkill.exe" /F /IM "${GUI_EXE}"'
    Pop $0
    Sleep 500
FunctionEnd
!macroend
!insertmacro StopProgram ""
!insertmacro StopProgram "un."

; Waits, ten seconds at most, until the file on the stack can be opened for
; writing. The service manager reports "stopped" before the process has let go
; of its own image, and NSIS answers a file it cannot replace with an
; Abort/Retry/Ignore box at once — which a silent run takes as "abort".
!macro WaitWritable UN
Function ${UN}WaitWritable
    Exch $R0
    Push $R1
    Push $R2
    StrCpy $R2 0
    ${DoWhile} $R2 < 40
        ${IfNot} ${FileExists} "$R0"
            ${ExitDo}
        ${EndIf}
        ClearErrors
        FileOpen $R1 "$R0" a
        ${IfNot} ${Errors}
            FileClose $R1
            ${ExitDo}
        ${EndIf}
        Sleep 250
        IntOp $R2 $R2 + 1
    ${Loop}
    ClearErrors
    Pop $R2
    Pop $R1
    Pop $R0
FunctionEnd
!macroend
!insertmacro WaitWritable ""
!insertmacro WaitWritable "un."

; Started through Explorer so that the elevated installer does not hand its
; administrator rights to the window: it is meant to run as the person at the
; desk, and everything privileged is the service's business.
Function RunProgram
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\${GUI_EXE}"'
FunctionEnd

; An update goes where the program already is. Two copies would share one
; service, and the service can only be one of them.
Function SkipDirectoryOnUpgrade
    ${If} $Upgrade == "yes"
        Abort
    ${EndIf}
FunctionEnd

; --------------------------------------------------------------- sections

Section "!${NAME}" SecMain
    SectionIn RO
    ${GetRoot} "$INSTDIR" $0
    ${If} $0 == ""
    ${OrIf} $INSTDIR == "$0"
    ${OrIf} $INSTDIR == "$0\"
        SetErrorLevel 87
        Abort "$(BadDir)"
    ${EndIf}
    SetShellVarContext all

    Call StopProgram

    ; A registered service holds its executable open, wherever it runs from.
    ; It is stopped by the executable it is registered with — the one already
    ; on this computer — which waits for a running backup to wind down.
    ;
    ; The first version unpacked the NEW executable into %TEMP% as
    ; service-control.exe and ran that, so as not to depend on an old copy.
    ; An unsigned program nobody has seen before, appearing in the temporary
    ; folder under another name and talking to the service manager, is what an
    ; antivirus heuristic is written to catch: Avast held it for analysis and
    ; the installer stood at this line (2026-10-06). Nothing is run from %TEMP%
    ; now. A registration whose file is gone has nothing running to stop.
    ReadRegStr $1 HKLM "${SERVICE_KEY}" "ImagePath"
    StrCpy $2 $1 1
    ${If} $2 == '"'
        StrCpy $1 $1 "" 1
        StrLen $2 $1
        IntOp $2 $2 - 1
        StrCpy $1 $1 $2
    ${EndIf}
    ${If} $1 != ""
    ${AndIf} ${FileExists} "$1"
        DetailPrint "$(StoppingService)"
        nsExec::Exec '"$1" /stop'
        Pop $0
        ${If} $0 != "0"
            Abort "$(ServiceStopFailed)"
        ${EndIf}
    ${EndIf}
    Push "$INSTDIR\${SERVICE_EXE}"
    Call WaitWritable
    Push "$INSTDIR\${GUI_EXE}"
    Call WaitWritable

    SetOutPath "$INSTDIR"
    File "${GUI_EXE}"
    File "${SERVICE_EXE}"
    File "${CLI_EXE}"
    File "${SHELL_EXE}"
    File "LICENSE.txt"
    File "NOTICE.txt"
    File "COMMERCIAL-LICENSE.txt"
    File "THIRD-PARTY-NOTICES.txt"
    File "THIRD-PARTY-LICENSES.txt"

    ; Total Commander may have the plugin loaded, and a loaded DLL cannot be
    ; replaced — but it can be renamed. The old one goes at the next restart.
    SetOutPath "$INSTDIR\${TC_DIR}"
    Delete "$INSTDIR\${TC_DIR}\${TC_PLUGIN}.old"
    Rename "$INSTDIR\${TC_DIR}\${TC_PLUGIN}" "$INSTDIR\${TC_DIR}\${TC_PLUGIN}.old"
    File "${TC_DIR}\${TC_PLUGIN}"
    File "${TC_DIR}\pluginst.inf"
    Delete /REBOOTOK "$INSTDIR\${TC_DIR}\${TC_PLUGIN}.old"
    SetOutPath "$INSTDIR"

    ; Before the service is touched, so that a failure below still leaves a
    ; program Windows knows how to remove.
    WriteUninstaller "$INSTDIR\uninstall.exe"
    WriteRegStr HKLM "${APP_KEY}" "InstallDir" "$INSTDIR"
    ; The wizard remembers its language when its last page is left. A silent
    ; run has no pages, and the uninstaller of a silently installed copy then
    ; opened with a "Please select a language" dialog — so it is written here.
    WriteRegStr HKLM "${APP_KEY}" "${MUI_LANGDLL_REGISTRY_VALUENAME}" "$LANGUAGE"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayName" "${NAME}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\${GUI_EXE}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "NoRepair" 1
    ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
    IntFmt $0 "0x%08X" $0
    WriteRegDWORD HKLM "${UNINSTALL_KEY}" "EstimatedSize" "$0"

    CreateDirectory "$SMPROGRAMS\${NAME}"
    CreateShortcut "$SMPROGRAMS\${NAME}\${NAME}.lnk" "$INSTDIR\${GUI_EXE}"

    ; The service registers itself, and when it is registered already it
    ; points the registration at this copy (service/src/install.rs). Its
    ; LocalSystem account, automatic start and the quotes around a path with
    ; spaces are decided there, in one place.
    DetailPrint "$(RegisteringService)"
    nsExec::Exec '"$INSTDIR\${SERVICE_EXE}" /install'
    Pop $0
    ${If} $0 != "0"
        Abort "$(ServiceInstallFailed)"
    ${EndIf}
    DetailPrint "$(StartingService)"
    nsExec::Exec '"$INSTDIR\${SERVICE_EXE}" /start'
    Pop $0
    ${If} $0 != "0"
        ; Installed and removable; say so instead of undoing it.
        SetErrorLevel 3
        MessageBox MB_OK|MB_ICONEXCLAMATION "$(ServiceStartFailed)" /SD IDOK
    ${EndIf}
SectionEnd

Section /o "$(NameDesktop)" SecDesktop
    SetShellVarContext all
    CreateShortcut "$DESKTOP\${NAME}.lnk" "$INSTDIR\${GUI_EXE}"
SectionEnd

; The window is a WebView2 page. Windows 11 has the runtime; a Windows 10 that
; never saw Edge may not. The window itself explains this when it starts, but
; here is where the person can still do something about it in one click. The
; page is opened through Explorer for the same reason the program is.
Section "-WebView2"
    ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\${WEBVIEW2_CLIENT}" "pv"
    ${If} $0 == ""
    ${OrIf} $0 == "0.0.0.0"
        ReadRegStr $0 HKCU "Software\Microsoft\EdgeUpdate\Clients\${WEBVIEW2_CLIENT}" "pv"
    ${EndIf}
    ${If} $0 == ""
    ${OrIf} $0 == "0.0.0.0"
        MessageBox MB_YESNO|MB_ICONEXCLAMATION "$(NoWebView2)" /SD IDNO IDNO skip
        Exec '"$WINDIR\explorer.exe" "${WEBVIEW2_URL}"'
        skip:
    ${EndIf}
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "$(DescMain)"
    !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "$(DescDesktop)"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Function .onInit
    ${IfNot} ${RunningX64}
        MessageBox MB_OK|MB_ICONSTOP "$(Need64)" /SD IDOK
        Abort
    ${EndIf}
    SetRegView 64
    SetShellVarContext all

    ; An earlier installation decides the folder: the Directory page is
    ; skipped and a /D= on the command line is not followed. To move the
    ; program, remove it and install it again.
    StrCpy $Upgrade "no"
    ReadRegStr $0 HKLM "${UNINSTALL_KEY}" "InstallLocation"
    ${If} $0 != ""
    ${AndIf} ${FileExists} "$0\${SERVICE_EXE}"
        StrCpy $Upgrade "yes"
        StrCpy $INSTDIR $0
    ${ElseIf} $INSTDIR == ""
        StrCpy $INSTDIR "$PROGRAMFILES64\${SHORT_NAME}"
    ${EndIf}

    !insertmacro MUI_LANGDLL_DISPLAY

    ; A desktop shortcut made last time is kept up to date.
    ${If} $Upgrade == "yes"
    ${AndIf} ${FileExists} "$DESKTOP\${NAME}.lnk"
        !insertmacro SelectSection ${SecDesktop}
    ${EndIf}
FunctionEnd

Function un.onInit
    SetRegView 64
    SetShellVarContext all
    !insertmacro MUI_UNGETLANGUAGE
FunctionEnd

; ------------------------------------------------------------- uninstall

Section "Uninstall"
    ${un.GetRoot} "$INSTDIR" $0
    ${If} $0 == ""
    ${OrIf} $INSTDIR == "$0"
    ${OrIf} $INSTDIR == "$0\"
        SetErrorLevel 87
        Abort "$(BadDir)"
    ${EndIf}
    SetShellVarContext all

    Call un.StopProgram
    Push "$INSTDIR\${GUI_EXE}"
    Call un.WaitWritable

    ; The service goes only if it is THIS copy's. A developer's build
    ; registered from another folder is not this uninstaller's to remove.
    ; The registration is one quoted path with no arguments.
    ReadRegStr $1 HKLM "${SERVICE_KEY}" "ImagePath"
    StrCpy $2 $1 1
    ${If} $2 == '"'
        StrCpy $1 $1 "" 1
        StrLen $2 $1
        IntOp $2 $2 - 1
        StrCpy $1 $1 $2
    ${EndIf}
    GetFullPathName $2 "$1"
    GetFullPathName $3 "$INSTDIR\${SERVICE_EXE}"
    ClearErrors
    StrCpy $ForeignService "no"
    ${If} $3 != ""
    ${AndIf} $2 == $3
        DetailPrint "$(RemovingService)"
        ; `/uninstall` gives a running service six seconds; `/stop` waits for
        ; a backup to wind down, and nothing below can go ahead until it has.
        nsExec::Exec '"$INSTDIR\${SERVICE_EXE}" /stop'
        Pop $0
        ${If} $0 != "0"
            Abort "$(ServiceStopFailed)"
        ${EndIf}
        nsExec::Exec '"$INSTDIR\${SERVICE_EXE}" /uninstall'
        Pop $0
        ${If} $0 != "0"
            Abort "$(ServiceRemoveFailed)"
        ${EndIf}
        Push "$INSTDIR\${SERVICE_EXE}"
        Call un.WaitWritable
    ${ElseIf} $1 != ""
        StrCpy $ForeignService "yes"
        DetailPrint "$(ForeignService)"
        DetailPrint "$1"
    ${EndIf}

    ; Asked here, while the service executable is still on disk: deleting the
    ; settings is its work. They belong to the service, so one that stays —
    ; another copy's — keeps them and nobody is asked.
    ${If} $ForeignService == "no"
        Call un.AskAboutUserData
    ${EndIf}

    ; The icon's logon entry of whoever is uninstalling, if it is this copy's.
    ; Other accounts keep an entry that points at nothing, which Windows skips.
    ReadRegStr $0 HKCU "${RUN_KEY}" "${RUN_VALUE}"
    ${If} $0 == '"$INSTDIR\${GUI_EXE}" --tray'
        DeleteRegValue HKCU "${RUN_KEY}" "${RUN_VALUE}"
    ${EndIf}

    Delete "$SMPROGRAMS\${NAME}\${NAME}.lnk"
    RMDir "$SMPROGRAMS\${NAME}"
    Delete "$DESKTOP\${NAME}.lnk"

    Delete "$INSTDIR\${GUI_EXE}"
    Delete "$INSTDIR\${SERVICE_EXE}"
    Delete "$INSTDIR\${CLI_EXE}"
    Delete "$INSTDIR\${SHELL_EXE}"
    Delete "$INSTDIR\LICENSE.txt"
    Delete "$INSTDIR\NOTICE.txt"
    Delete "$INSTDIR\COMMERCIAL-LICENSE.txt"
    Delete "$INSTDIR\THIRD-PARTY-NOTICES.txt"
    Delete "$INSTDIR\THIRD-PARTY-LICENSES.txt"
    Delete /REBOOTOK "$INSTDIR\${TC_DIR}\${TC_PLUGIN}"
    Delete /REBOOTOK "$INSTDIR\${TC_DIR}\${TC_PLUGIN}.old"
    Delete "$INSTDIR\${TC_DIR}\pluginst.inf"
    RMDir "$INSTDIR\${TC_DIR}"
    RMDir /r "$INSTDIR\Logs"

    DeleteRegKey HKLM "${UNINSTALL_KEY}"
    DeleteRegKey HKLM "${APP_KEY}"

    ; Last, so a failure above still leaves a way to remove the program.
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"
SectionEnd

; Jobs, history and remembered volume keys are worth keeping across a
; reinstall, so removing them is a separate answer, and a silent uninstall
; keeps them. Backup images are somewhere else and are never touched.
Function un.AskAboutUserData
    ${IfNot} ${FileExists} "${DATA_DIR}\*.*"
        Return
    ${EndIf}
    MessageBox MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2 "$(AskRemoveData)" /SD IDNO IDYES remove
    DetailPrint "$(KeptData)"
    DetailPrint "${DATA_DIR}"
    Return
    remove:
    ; The service's own executable deletes its data. Part of it — the
    ; remembered volume keys, of all things — is closed to everyone but
    ; LocalSystem, and an ordinary delete from here removed the rest and left
    ; exactly that part (service/src/install.rs, purge_data). The RMDir after it
    ; is for an executable too old to know the command.
    nsExec::Exec '"$INSTDIR\${SERVICE_EXE}" /purge-data'
    Pop $0
    RMDir /r "${DATA_DIR}"
    ${If} ${FileExists} "${DATA_DIR}\*.*"
        DetailPrint "$(DataRemoveFailed)"
        DetailPrint "${DATA_DIR}"
        MessageBox MB_OK|MB_ICONEXCLAMATION "$(DataRemoveFailed)$\r$\n${DATA_DIR}" /SD IDOK
    ${EndIf}
    DeleteRegKey HKCU "${APP_KEY}"
    ; The window's own storage: the theme, the language, which job was open.
    ; It is one person's, under their profile, and outlives the program — a
    ; copy installed later opened in the theme chosen in the removed one.
    ; Only the profile of whoever is uninstalling can be reached from here.
    SetShellVarContext current
    RMDir /r "$LOCALAPPDATA\${WEBVIEW_DATA}"
    SetShellVarContext all
FunctionEnd
