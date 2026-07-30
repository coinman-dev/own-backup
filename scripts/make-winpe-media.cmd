@echo off
setlocal enabledelayedexpansion
rem ===========================================================================
rem  Boot-media spike (stage 0 of .ai/boot-media-plan_2026-07-30.md).
rem
rem  Builds a WinPE medium out of what this machine already has:
rem    * base       - Winre.wim (the Windows recovery environment);
rem    * payload    - our exes next to this script + the machine's WebView2;
rem    * boot chain - bootmgfw.efi and the READY-MADE BCD store that ships with
rem                   Windows (%SystemRoot%\Boot\DVD\EFI\BCD boots
rem                   \sources\boot.wim off a ramdisk).
rem  Downloads nothing. Needs no ADK.
rem
rem  Run from an ELEVATED console:
rem      make-winpe-media.cmd [folder_with_exes] [output_folder]
rem  Defaults: exes and output both next to this script (.\OwnBackupMedia).
rem
rem  Two things about how it works:
rem
rem  * All output is ASCII on purpose: a .cmd is read in the console's OEM code
rem    page, and anything else comes out as mojibake.
rem
rem  * The image is edited by unpacking and repacking it (/Apply-Image +
rem    /Capture-Image) rather than by mounting it. Mounting needs the wimmount
rem    filter driver, which trimmed-down Windows builds are missing (error 1243,
rem    "the specified service does not exist"). Mounting is still tried first
rem    because it is faster; the fallback is automatic. The unpacked tree is a
rem    scratch directory - it is deleted when the medium is built.
rem ===========================================================================

net session >nul 2>&1
if errorlevel 1 (
  echo [!] Administrator rights required - run from an elevated console.
  exit /b 1
)

set "HERE=%~dp0"
if "%HERE:~-1%"=="\" set "HERE=%HERE:~0,-1%"
set "SRC=%HERE%"
if not "%~1"=="" set "SRC=%~1"
set "OUT=%HERE%\OwnBackupMedia"
if not "%~2"=="" set "OUT=%~2"

echo === My Own Backup boot medium (spike) ===
echo     program : %SRC%
echo     output  : %OUT%
echo.

rem --- 1. our binaries -------------------------------------------------------
for %%F in (own-backup-gui.exe own-backup-service.exe own-backup-cli.exe) do (
  if not exist "%SRC%\%%F" (
    echo [!] %%F not found in "%SRC%".
    echo     Put this script next to the exes, or pass their folder as arg 1.
    exit /b 1
  )
)

rem --- 2. base image: Winre.wim ----------------------------------------------
set "BASE="
if exist "%SystemRoot%\System32\Recovery\Winre.wim" set "BASE=%SystemRoot%\System32\Recovery\Winre.wim"
if not defined BASE if exist "%SystemDrive%\Recovery\WindowsRE\Winre.wim" set "BASE=%SystemDrive%\Recovery\WindowsRE\Winre.wim"
if not defined BASE (
  echo [!] Winre.wim not found. Either:
  echo       - enable the recovery environment:  reagentc /enable
  echo       - or take it out of an installation ISO:
  echo           dism /Mount-Wim /WimFile:D:\sources\install.wim /Index:1 /MountDir:C:\mnt /ReadOnly
  echo           copy C:\mnt\Windows\System32\Recovery\Winre.wim %SystemRoot%\System32\Recovery\
  echo           dism /Unmount-Wim /MountDir:C:\mnt /Discard
  exit /b 1
)
echo [1/7] base     : %BASE%

rem --- 3. WebView2 runtime ---------------------------------------------------
set "WV="
for /f "delims=" %%D in ('dir /b /ad /o-n "%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\1*" 2^>nul') do (
  if not defined WV set "WV=%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\%%D"
)
if not defined WV for /f "delims=" %%D in ('dir /b /ad /o-n "%ProgramFiles%\Microsoft\EdgeWebView\Application\1*" 2^>nul') do (
  if not defined WV set "WV=%ProgramFiles%\Microsoft\EdgeWebView\Application\%%D"
)
if not defined WV if exist "%SRC%\WebView2\msedgewebview2.exe" set "WV=%SRC%\WebView2"
if not defined WV (
  echo [!] WebView2 runtime not found. Install Microsoft Edge WebView2 Runtime,
  echo     or drop its files into "%SRC%\WebView2\" ^(msedgewebview2.exe needed^).
  exit /b 1
)
echo [2/7] webview2 : %WV%

rem --- 4. work area ----------------------------------------------------------
set "WORK=%OUT%\work"
set "TREE=%OUT%\tree"
if exist "%TREE%\Windows" dism /Unmount-Wim /MountDir:"%TREE%" /Discard >nul 2>&1
call :nuke "%OUT%"
mkdir "%WORK%" 2>nul
mkdir "%TREE%" 2>nul

copy /y "%BASE%" "%WORK%\boot.wim" >nul || (echo [!] cannot copy the base image & exit /b 1)
attrib -r -h -s "%WORK%\boot.wim"

rem --- 5. open the image: mount, or unpack if mounting is unavailable ---------
set "MODE=mount"
echo [3/7] opening the image ^(mount^)...
dism /Mount-Wim /WimFile:"%WORK%\boot.wim" /Index:1 /MountDir:"%TREE%" >nul 2>&1
if errorlevel 1 (
  rem Error 1243 means the wimmount filter driver has no service registered -
  rem trimmed-down Windows builds ship the driver but not its registration.
  rem One INF install fixes it, and then there is no unpacked tree at all.
  if exist "%SystemRoot%\System32\drivers\wimmount.inf" (
    echo       mount failed - registering the wimmount driver and retrying...
    rundll32.exe setupapi.dll,InstallHinfSection DefaultInstall 132 "%SystemRoot%\System32\drivers\wimmount.inf" >nul 2>&1
    dism /Mount-Wim /WimFile:"%WORK%\boot.wim" /Index:1 /MountDir:"%TREE%" >nul 2>&1
  )
)
if errorlevel 1 (
  echo       mount unavailable ^(wimmount driver missing?^) - unpacking instead,
  echo       this takes a few minutes and a couple of GB of disk.
  set "MODE=apply"
  dism /Apply-Image /ImageFile:"%WORK%\boot.wim" /Index:1 /ApplyDir:"%TREE%" || (
    echo [!] Could not open the image at all. See %SystemRoot%\Logs\DISM\dism.log
    exit /b 1
  )
)

rem --- 6. payload ------------------------------------------------------------
set "APP=%TREE%\Program Files\OwnBackup"
mkdir "%APP%" 2>nul
mkdir "%APP%\WebView2" 2>nul
copy /y "%SRC%\own-backup-gui.exe" "%APP%\" >nul
copy /y "%SRC%\own-backup-service.exe" "%APP%\" >nul
copy /y "%SRC%\own-backup-cli.exe" "%APP%\" >nul
echo [4/7] copying WebView2 ^(about 250 MB^)...
xcopy "%WV%\*" "%APP%\WebView2\" /e /i /y /q >nul

rem WinPE boots from X:, has no desktop compositor and no sandbox, and the
rem browser profile has to sit on a writable volume.
> "%APP%\start-media.cmd" echo @echo off
>>"%APP%\start-media.cmd" echo wpeinit
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=X:\Program Files\OwnBackup\WebView2"
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--no-sandbox --disable-gpu --disable-gpu-compositing"
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_USER_DATA_FOLDER=X:\Windows\Temp\OwnBackupWebView2"
>>"%APP%\start-media.cmd" echo md "%%WEBVIEW2_USER_DATA_FOLDER%%" 2^>nul
>>"%APP%\start-media.cmd" echo start "" "X:\Program Files\OwnBackup\own-backup-service.exe" --run
>>"%APP%\start-media.cmd" echo "X:\Program Files\OwnBackup\own-backup-gui.exe"
>>"%APP%\start-media.cmd" echo echo GUI exited - console left open on purpose.
>>"%APP%\start-media.cmd" echo cmd

rem Our launcher instead of the recovery environment's own shell.
> "%TREE%\Windows\System32\winpeshl.ini" echo [LaunchApps]
>>"%TREE%\Windows\System32\winpeshl.ini" echo "X:\Program Files\OwnBackup\start-media.cmd"

rem WinPE's default 32 MB scratch space is not enough for a browser engine.
if "%MODE%"=="mount" (
  dism /Image:"%TREE%" /Set-ScratchSpace:512 >nul
) else (
  reg load HKLM\OwnBackupWpe "%TREE%\Windows\System32\config\SYSTEM" >nul 2>&1
  reg add HKLM\OwnBackupWpe\ControlSet001\Services\FBWF /v WinPECacheThreshold /t REG_DWORD /d 512 /f >nul 2>&1
  reg unload HKLM\OwnBackupWpe >nul 2>&1
)

rem --- 7. close the image ----------------------------------------------------
echo [5/7] writing the image back...
if "%MODE%"=="mount" (
  dism /Unmount-Wim /MountDir:"%TREE%" /Commit || (
    echo [!] Could not commit the image. See %SystemRoot%\Logs\DISM\dism.log
    exit /b 1
  )
) else (
  del /f /q "%WORK%\boot.wim"
  dism /Capture-Image /ImageFile:"%WORK%\boot.wim" /CaptureDir:"%TREE%" ^
       /Name:"My Own Backup media" /Compress:max /Bootable || (
    echo [!] Could not capture the image. See %SystemRoot%\Logs\DISM\dism.log
    exit /b 1
  )
)
call :nuke "%TREE%"
if exist "%TREE%" (
  echo       [!] scratch tree survived - see the note at the end
) else (
  if "%MODE%"=="apply" echo       scratch tree removed
)

rem --- 8. media layout (UEFI) -------------------------------------------------
echo [6/7] laying out the medium...
set "MEDIA=%OUT%\media"
mkdir "%MEDIA%\EFI\BOOT" 2>nul
mkdir "%MEDIA%\EFI\Microsoft\Boot" 2>nul
mkdir "%MEDIA%\boot" 2>nul
mkdir "%MEDIA%\sources" 2>nul
copy /y "%SystemRoot%\Boot\EFI\bootmgfw.efi" "%MEDIA%\EFI\BOOT\BOOTX64.EFI" >nul
copy /y "%SystemRoot%\Boot\DVD\EFI\BCD" "%MEDIA%\EFI\Microsoft\Boot\BCD" >nul
copy /y "%SystemRoot%\Boot\DVD\EFI\boot.sdi" "%MEDIA%\boot\boot.sdi" >nul
move /y "%WORK%\boot.wim" "%MEDIA%\sources\boot.wim" >nul
call :nuke "%WORK%"

if exist "%TREE%" call :nuke "%TREE%"
if exist "%WORK%" call :nuke "%WORK%"

echo [7/7] done.
echo.
echo Medium assembled in: %MEDIA%
dir /s /b "%MEDIA%"
if exist "%TREE%" (
  echo.
  echo [!] The scratch tree could not be removed: %TREE%
  echo     Remove it by hand with ^(backup mode ignores the ACLs^):
  echo       md "%%TEMP%%\empty"
  echo       robocopy "%%TEMP%%\empty" "%TREE%" /MIR /B
  echo       rmdir /s /q "%TREE%" ^& rmdir /s /q "%%TEMP%%\empty"
)
echo.
echo Next, either:
echo   * USB stick: diskpart - clean / convert gpt / create partition primary /
echo     format fs=fat32 quick / assign, then copy everything from "%MEDIA%"
echo     into its root;
echo   * virtual machine: copy the "%MEDIA%" folder to
echo     \\wsl.localhost\Ubuntu\home\modulator\Development\own-backup\tmp\
echo     and a bootable .vmdk will be built out of it.
echo.
echo If mounting failed above and you want it working ^(it is faster^), the WIM
echo mount filter driver can be registered with:
echo   rundll32.exe setupapi.dll,InstallHinfSection DefaultInstall 132 %SystemRoot%\System32\drivers\wimmount.inf
echo   sc query wimmount
endlocal
exit /b 0

rem ---------------------------------------------------------------------------
rem  Delete a folder that came out of a Windows image.
rem
rem  Applied system files keep the ACLs they had inside the image: owned by
rem  TrustedInstaller, with administrators holding read only. Nothing is
rem  "locked" - there is simply no permission to delete, so ownership has to be
rem  taken first. `icacls /setowner` is used rather than `takeown`, whose /D
rem  answer is localised and refuses the English "Y" on a localised Windows.
rem ---------------------------------------------------------------------------
:nuke
if not exist "%~1" exit /b 0
rmdir /s /q "%~1" >nul 2>&1
if not exist "%~1" exit /b 0
dism /Cleanup-Mountpoints >nul 2>&1

rem Backup mode first: robocopy /B opens through SeBackup/SeRestore, which
rem administrators hold, and those privileges override the ACL instead of
rem arguing with it. Mirroring an empty folder over the tree empties it.
set "EMPTY=%TEMP%\ownbackup-empty"
mkdir "%EMPTY%" 2>nul
robocopy "%EMPTY%" "%~1" /MIR /B /R:1 /W:1 /NFL /NDL /NJH /NJS /NP >nul 2>&1
rmdir /s /q "%EMPTY%" >nul 2>&1
rmdir /s /q "%~1" >nul 2>&1
if not exist "%~1" exit /b 0

rem Then ownership. `takeown` rather than `icacls /setowner`: taking ownership
rem needs SeTakeOwnership *enabled*, and only takeown does that - which is why
rem icacls answers "access denied" on WinSxS manifests.
takeown /f "%~1" /r /a /skipsl >nul 2>&1
icacls "%~1" /grant "*S-1-5-32-544:(OI)(CI)F" /t /c /q >nul 2>&1
attrib -r -h -s "%~1\*.*" /s /d >nul 2>&1
for /l %%N in (1,1,3) do (
  rmdir /s /q "%~1" >nul 2>&1
  if not exist "%~1" exit /b 0
  ping -n 3 127.0.0.1 >nul
)
exit /b 1
