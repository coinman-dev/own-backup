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
rem  The interface starts at boot (2026-07-31, once the window was working). The
rem  rule that survives from the medium that trapped its user is not "start
rem  nothing" but "hold nothing": that first one ran the interface in the
rem  FOREGROUND of the only console, and when the window failed to open the
rem  process stayed resident and the console was gone - nothing could be typed
rem  or inspected. Everything is launched with `start` now, so this console
rem  comes back to a prompt whether the interface appears or not.
rem
rem  Why the interface showed no window (found 2026-07-31, own-backup-gui.log):
rem  WebView2 creation failed with 0x80080005 - the browser process refused to
rem  start. WinPE runs everything as SYSTEM and WebView2 refuses SYSTEM unless
rem  --allow-run-as-system is passed; the sandbox also cannot start there. Both
rem  flags were set via WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS, but wry (the
rem  Tauri runtime) always sets additionalBrowserArguments through the WebView2
rem  API, and the API value makes the loader IGNORE that variable. The GUI now
rem  passes the variable through the API itself - this medium needs a GUI built
rem  with build_tag winpe-2026-07-31a-browser-args-via-api or newer (the tag is
rem  printed at the top of own-backup-gui.log). WinPE also lacks the media/D3D
rem  DLLs the browser links against; they are copied in below.
rem
rem  Why the window was then black (found 2026-07-31, webview2.log): even with
rem  --disable-gpu --disable-gpu-compositing, Chromium still PRESENTS the
rem  software-composited frame through a DXGI swap chain whenever it thinks
rem  DirectComposition is available. WinPE has no video device, so every
rem  Present1/Map returned DXGI_ERROR_DEVICE_REMOVED (0x887A0005), the display
rem  compositor crashed in a loop, and no frame ever reached the window - while
rem  the page itself ran fine (the log shows list_disks answering).
rem
rem  THE FIX (confirmed on a boot 2026-07-31): do not put dcomp.dll in the image.
rem  It was OUR OWN doing - the DLL came from the list this script copies out of
rem  the build host's System32, inherited from an earlier WebView2 workaround.
rem  Native WinRE ships no dcomp.dll (it does ship d3d11/dxgi/d3d9/d3d10warp),
rem  and Edge picks the presenter by trying to CREATE a DirectComposition device,
rem  not by reading any switch. With the DLL present that probe succeeds, the
rem  composition swap chain is chosen (CreateSwapChainForComposition), and with
rem  no DWM to compose with every Present1 dies. dcomp.dll is a delay-import of
rem  msedge.dll, so with the DLL gone the probe fails cleanly and Edge falls back
rem  to a plain GDI blit - which is what WinPE can actually do. See the `del`
rem  below; the DLL is also kept out of the copy list.
rem
rem  What did NOT fix it, because the lesson is the useful part. First
rem  --disable-direct-composition: the presenter is chosen in the GPU PROCESS,
rem  and that switch is not among the ones the browser forwards there
rem  (kSwitchNames in content/browser/gpu/gpu_process_host.cc), so the check
rem  never sees it. Then --disable-features=RemoveRedirectionBitmap: feature
rem  overrides DO reach every child, but reversing msedge.dll showed that gate
rem  (features::ShouldRemoveRedirectionBitmap) has exactly one code reference and
rem  begins with a Win11-22H2-or-newer test, which WinPE 19041 fails anyway - so
rem  it was never the gate in play. Then --in-process-gpu, to make the browser's
rem  own command line be the one the decision is made from: no change either,
rem  since the decision is a capability probe, not a switch. Both are still
rem  passed below - they cost nothing and cover other builds - but the DLL is
rem  what mattered. When a Chromium switch does nothing, ask what it is really
rem  probing.
rem
rem  Those browser arguments live in a text file rather than being baked into
rem  the launcher, so a flag can be tried without rebuilding a 760 MB image:
rem  put "ownbackup-webview2-args.txt" in the root of any drive the medium can
rem  see (C: to H:) and it wins over the copy inside the image.
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

echo === Own Backup boot medium (spike) ===
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

rem WinPE carries only the DLLs its own tools need. Anything our binaries or
rem WebView2 import beyond that has to travel with them, or the process dies at
rem load time before its first line of code - printing nothing at all, which is
rem exactly how the first medium behaved (2026-07-31). Our exes are linked
rem against the static CRT now, so the C runtime is no longer among them. The
rem browser process additionally links Media Foundation, Direct3D/DirectWrite
rem and the WinInet family, none of which WinRE carries. WinRE comes from this
rem same Windows installation, so DLLs taken out of System32 match the image's
rem OS build; nothing the image already has is ever overwritten.
for %%D in (virtdisk.dll newdev.dll dwmapi.dll comctl32.dll shlwapi.dll cfgmgr32.dll
            setupapi.dll crypt32.dll bcrypt.dll bcryptprimitives.dll
            mf.dll mfplat.dll mfreadwrite.dll mfcore.dll mfsrcsnk.dll mfperfhelper.dll evr.dll
            d3d11.dll d3d10_1.dll d3d10.dll d3d9.dll dxgi.dll d2d1.dll dwrite.dll
            d3dcompiler_47.dll windowscodecs.dll opengl32.dll
            propsys.dll userenv.dll winhttp.dll wininet.dll urlmon.dll iertutil.dll
            wintrust.dll cryptui.dll ncrypt.dll version.dll winmm.dll usp10.dll) do (
  if not exist "%TREE%\Windows\System32\%%D" (
    if exist "%SystemRoot%\System32\%%D" (
      copy /y "%SystemRoot%\System32\%%D" "%TREE%\Windows\System32\" >nul
      echo       added missing %%D
    )
  )
)
for %%D in (mf.dll mfplat.dll d3d11.dll dxgi.dll d2d1.dll dwrite.dll) do (
  if not exist "%TREE%\Windows\System32\%%D" (
    echo       [!] %%D is unavailable - WebView2 will most likely not start
  )
)
rem Trim the runtime. Measured on the medium (2026-07-31) it is 849 MB unpacked,
rem and WinPE loads the whole boot.wim into RAM, so what is dropped here is RAM
rem the machine gets to keep. Only dead weight goes:
rem   * Locales - 168 languages, 120 MB; Chromium falls back to en-US for any
rem     language whose .pak is absent, and our interface ships ru/en anyway;
rem   * ResiliencyLinks - a byte-identical SECOND copy of Locales and the .pak
rem     files, kept for Edge's self-repair (verified with cmp). Note that the
rem     WIM stores identical data once, so deleting one copy alone frees
rem     NOTHING - the locales must go from both places, which is why this runs
rem     before the image is written;
rem   * WidevineCdm - DRM video playback, and Installer / edge_game_assist -
rem     updater and gaming overlay. None of them can be reached from our UI.
rem Left alone on purpose: msedge.dll (316 MB) is the engine itself, and the
rem loose DLLs (mspdf, dxcompiler, onnxruntime, vk_swiftshader) are loaded by
rem the browser process on paths we do not fully control - a missing one would
rem fail at run time, on the user's machine, for 60 MB.
for %%D in (ResiliencyLinks WidevineCdm Installer edge_game_assist) do (
  if exist "%APP%\WebView2\%%D" rd /s /q "%APP%\WebView2\%%D"
)
if exist "%APP%\WebView2\Locales" (
  for /f "delims=" %%L in ('dir /b "%APP%\WebView2\Locales\*.pak" 2^>nul') do (
    if /i not "%%L"=="ru.pak" if /i not "%%L"=="en-US.pak" del /f /q "%APP%\WebView2\Locales\%%L"
  )
)

rem dcomp.dll must NOT be in the image, and an earlier build put it there, so it
rem is deleted explicitly. Reversed from msedge.dll (Edge 150, 2026-07-31): the
rem presenter is picked by probing whether a DirectComposition device can be
rem created, not by any switch. dcomp.dll is a delay-import; when the DLL is
rem absent the probe fails cleanly and Edge falls back to plain GDI - which is
rem the only path that can work with no DWM running. With the DLL present the
rem client-side device gets created, the composition swap chain is chosen, and
rem every Present1 dies with DXGI_ERROR_DEVICE_REMOVED (the black window with
rem list_disks working underneath). Original WinRE ships no dcomp.dll either.
del /f /q "%TREE%\Windows\System32\dcomp.dll" >nul 2>&1

rem WebView2 is not ours and is not statically linked; it wants the VC runtime,
rem both next to msedgewebview2.exe and in its x64 subfolder.
for %%D in (vcruntime140.dll vcruntime140_1.dll msvcp140.dll) do (
  if exist "%SystemRoot%\System32\%%D" (
    copy /y "%SystemRoot%\System32\%%D" "%APP%\WebView2\" >nul
    if exist "%APP%\WebView2\x64" copy /y "%SystemRoot%\System32\%%D" "%APP%\WebView2\x64\" >nul
  )
)

rem The browser arguments. Only the FIRST line is read (start-media.cmd uses
rem "set /p"), so the rest of the file carries the alternatives to try if this
rem set is not enough - X: is a RAM disk, so swapping the top line with notepad
rem and re-running start-media.cmd needs neither a rebuild nor a reboot.
rem No Chromium logging by default. Asking for it AT ALL makes this Edge build
rem allocate a console for a process that has none, and that window lands over
rem the interface and fills with errors about services WinPE does not have (the
rem network-change watcher retries WSALookupServiceBegin once a second forever -
rem there is no Network Location Awareness here; the Edge LLM and geolocation
rem lines are the same story). Sending the log to a file with
rem --enable-logging=file cut the flood but not the window: the switch's presence
rem is what opens it. own-backup-gui.log is ours and always written, and it says
rem whether the webview was created - which is the question actually asked;
rem Chromium's log is one line away in the file below when it is needed.
rem RemoveRedirectionBitmap is the gate that picks the D3D presenter; the
rem msWebOOUI/msPdfOOUI/msSmartScreenProtection entries only restore wry's own
rem defaults, which the API value replaces.
> "%APP%\webview2-args.txt" echo --allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu --disable-gpu-compositing --disable-direct-composition --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired --no-first-run
>>"%APP%\webview2-args.txt" echo.
>>"%APP%\webview2-args.txt" echo Only the line above is used. To try another set, move its line to the top
>>"%APP%\webview2-args.txt" echo with notepad and run start-media.cmd again.
>>"%APP%\webview2-args.txt" echo.
>>"%APP%\webview2-args.txt" echo A - with Chromium's own log (opens a console window of its own):
>>"%APP%\webview2-args.txt" echo --allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu --disable-gpu-compositing --disable-direct-composition --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired --no-first-run --enable-logging=file --v=1 --log-file=X:\Windows\Temp\OwnBackup\webview2.log
>>"%APP%\webview2-args.txt" echo.
>>"%APP%\webview2-args.txt" echo B - the same, plus forcing software GL:
>>"%APP%\webview2-args.txt" echo --allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu --disable-gpu-compositing --disable-direct-composition --override-use-software-gl-for-tests --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired --no-first-run --enable-logging=file --v=1 --log-file=X:\Windows\Temp\OwnBackup\webview2.log
>>"%APP%\webview2-args.txt" echo.
>>"%APP%\webview2-args.txt" echo C - the other direction: give D3D a working device (WARP):
>>"%APP%\webview2-args.txt" echo --allow-run-as-system --no-sandbox --in-process-gpu --use-gl=angle --use-angle=d3d11-warp --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity --no-first-run --enable-logging=file --v=1 --log-file=X:\Windows\Temp\OwnBackup\webview2.log

rem The manual launcher. WinPE boots from X:, has no desktop compositor and no
rem sandbox, and the browser profile has to sit on a writable volume - all of
rem that is arranged here, right before the programs start. The interface goes
rem through `start` so this console is NEVER blocked by a windowless process.
rem The WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS value is passed to the WebView2
rem API by the GUI itself (see the header); it replaces wry's defaults, so the
rem msWebOOUI/msPdfOOUI/msSmartScreenProtection features are re-listed here.
set "SCRIPT=%APP%\start-media.cmd"
> "%SCRIPT%" echo @echo off
>>"%SCRIPT%" echo title Own Backup - boot medium
>>"%SCRIPT%" echo "X:\Program Files\OwnBackup\own-backup-cli.exe" display --min 1280x720
>>"%SCRIPT%" echo md "X:\Windows\Temp\OwnBackup" 2^>nul
>>"%SCRIPT%" echo md "X:\Windows\Temp\OwnBackup\Profile\AppData\Local" 2^>nul
>>"%SCRIPT%" echo md "X:\Windows\Temp\OwnBackup\Profile\AppData\Roaming" 2^>nul
>>"%SCRIPT%" echo md "X:\Windows\Temp\OwnBackup\WebView2Data" 2^>nul
>>"%SCRIPT%" echo set "TEMP=X:\Windows\Temp\OwnBackup"
>>"%SCRIPT%" echo set "TMP=X:\Windows\Temp\OwnBackup"
>>"%SCRIPT%" echo set "USERPROFILE=X:\Windows\Temp\OwnBackup\Profile"
>>"%SCRIPT%" echo set "LOCALAPPDATA=X:\Windows\Temp\OwnBackup\Profile\AppData\Local"
>>"%SCRIPT%" echo set "APPDATA=X:\Windows\Temp\OwnBackup\Profile\AppData\Roaming"
>>"%SCRIPT%" echo set "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=X:\Program Files\OwnBackup\WebView2"
>>"%SCRIPT%" echo set "WEBVIEW2_USER_DATA_FOLDER=X:\Windows\Temp\OwnBackup\WebView2Data"
>>"%SCRIPT%" echo set "ARGS_FILE=X:\Program Files\OwnBackup\webview2-args.txt"
>>"%SCRIPT%" echo for %%%%D in (C D E F G H) do if exist "%%%%D:\ownbackup-webview2-args.txt" set "ARGS_FILE=%%%%D:\ownbackup-webview2-args.txt"
>>"%SCRIPT%" echo set /p "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=" ^<"%%ARGS_FILE%%"
>>"%SCRIPT%" echo echo Browser arguments read from: %%ARGS_FILE%%
>>"%SCRIPT%" echo if not defined WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS echo [!] no arguments came out of that file
>>"%SCRIPT%" echo rem Media mode: state goes to the RAM disk instead of the rescued machine's
>>"%SCRIPT%" echo rem C:, the engine runs as a process (nothing to install into - WinPE's registry
>>"%SCRIPT%" echo rem lives in RAM), no VSS is attempted (the system here is offline, so a direct
>>"%SCRIPT%" echo rem read is already consistent) and schedules stay hidden. The programs detect
>>"%SCRIPT%" echo rem WinPE by themselves; this makes it explicit and covers child processes.
>>"%SCRIPT%" echo set "OWN_BACKUP_MEDIA=1"
>>"%SCRIPT%" echo rem WinPE's firewall resets loopback connections BETWEEN processes: the AIM
>>"%SCRIPT%" echo rem proxy (qcow2 mounting/exclusions) gets CONNECTION_REFUSED while same-process
>>"%SCRIPT%" echo rem probes pass. The service also does this itself at startup - belt and braces.
>>"%SCRIPT%" echo wpeutil disablefirewall ^>nul 2^>^&1
>>"%SCRIPT%" echo set "RUST_BACKTRACE=1"
>>"%SCRIPT%" echo set "RUST_LOG=info,own_backup_gui=debug,tauri_runtime_wry=debug,wry=debug"
rem Is there anything to start the interface WITH? WebView2 outside boot.wim,
rem room on the RAM disk, memory. A medium that cannot paint a window must say
rem so and leave a working console - that is what media-preflight prints, and
rem its exit code decides the branch below. The engine starts either way: the
rem CLI needs it just as much as the interface does.
>>"%SCRIPT%" echo del /f /q "%%TEMP%%\own-backup-gui-up" 2^>nul
>>"%SCRIPT%" echo echo Starting the engine...
>>"%SCRIPT%" echo start "own-backup engine" /min "X:\Program Files\OwnBackup\own-backup-service.exe" --run --media
>>"%SCRIPT%" echo "X:\Program Files\OwnBackup\own-backup-cli.exe" media-preflight
>>"%SCRIPT%" echo if errorlevel 1 goto :no_ui
>>"%SCRIPT%" echo echo Starting the interface...
>>"%SCRIPT%" echo start "" "X:\Program Files\OwnBackup\own-backup-gui.exe"
>>"%SCRIPT%" echo "X:\Program Files\OwnBackup\own-backup-cli.exe" media-wait --timeout 25
>>"%SCRIPT%" echo if errorlevel 1 goto :no_ui
>>"%SCRIPT%" echo echo.
>>"%SCRIPT%" echo echo The interface is up in its own window; this console stays usable.
>>"%SCRIPT%" echo goto :eof
>>"%SCRIPT%" echo :no_ui
>>"%SCRIPT%" echo echo.
>>"%SCRIPT%" echo echo Working without the interface. Logs:
>>"%SCRIPT%" echo echo   "X:\Program Files\OwnBackup\own-backup-gui.log"
>>"%SCRIPT%" echo echo   X:\Windows\Temp\OwnBackup\webview2.log
>>"%SCRIPT%" echo echo To try again after a fix: start-media.cmd

rem How WinPE starts what we want, and why it is startnet.cmd rather than
rem winpeshl.ini: entries in winpeshl.ini are handed to CreateProcess, which
rem cannot run a .cmd - the first medium booted straight into a bare console
rem because of it. With no winpeshl.ini the default shell runs
rem `cmd /k startnet.cmd`, which is a batch file by definition. The recovery
rem environment ships its own winpeshl.ini (it launches the recovery menu), so
rem that one has to go. startnet.cmd calls start-media.cmd, which launches
rem everything detached, so the boot console returns to a prompt immediately.
del /f /q "%TREE%\Windows\System32\winpeshl.ini" >nul 2>&1
set "SNET=%TREE%\Windows\System32\startnet.cmd"
> "%SNET%" echo @echo off
>>"%SNET%" echo wpeinit
>>"%SNET%" echo echo.
>>"%SNET%" echo echo === Own Backup ===
>>"%SNET%" echo echo.
>>"%SNET%" echo call "X:\Program Files\OwnBackup\start-media.cmd"
>>"%SNET%" echo echo.
>>"%SNET%" echo echo This console stays usable. To start the interface again:
>>"%SNET%" echo echo   "X:\Program Files\OwnBackup\start-media.cmd"

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
       /Name:"Own Backup media" /Compress:max /Bootable || (
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
echo The medium boots to a console; nothing of ours runs until you start it:
echo   X:\^> "X:\Program Files\OwnBackup\start-media.cmd"
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
