@echo off
setlocal enabledelayedexpansion
rem ===========================================================================
rem  Спайк загрузочного носителя (этап 0 плана .ai/boot-media-plan_2026-07-30.md)
rem
rem  Собирает WinPE-носитель из того, что уже есть на этой машине:
rem    * база          — Winre.wim (среда восстановления Windows);
rem    * интерфейс      — наши exe рядом с этим скриптом;
rem    * WebView2       — установленный на машине рантайм Edge WebView2;
rem    * загрузчик      — bootmgfw.efi и ГОТОВОЕ хранилище BCD из состава Windows
rem                       (C:\Windows\Boot\DVD\EFI\BCD грузит \sources\boot.wim).
rem  Ничего не скачивает и не требует ADK.
rem
rem  Запуск (от администратора):
rem      make-winpe-media.cmd [папка_с_exe] [папка_вывода]
rem  По умолчанию: exe берутся из папки скрипта, вывод — C:\OwnBackupMedia.
rem
rem  Цель спайка — один вопрос: рисуется ли окно интерфейса в WinPE.
rem ===========================================================================

net session >nul 2>&1
if errorlevel 1 (
  echo [!] Нужны права администратора — запустите из консоли "от имени администратора".
  exit /b 1
)

set "SRC=%~dp0"
if not "%~1"=="" set "SRC=%~1"
set "OUT=%SystemDrive%\OwnBackupMedia"
if not "%~2"=="" set "OUT=%~2"

echo === Загрузочный носитель My Own Backup (спайк) ===
echo     программа : %SRC%
echo     вывод     : %OUT%
echo.

rem --- 1. наши бинарники -----------------------------------------------------
for %%F in (own-backup-gui.exe own-backup-service.exe own-backup-cli.exe) do (
  if not exist "%SRC%\%%F" (
    echo [!] Не найден %%F в "%SRC%".
    echo     Положите скрипт рядом с exe или укажите папку первым аргументом.
    exit /b 1
  )
)

rem --- 2. база: Winre.wim ----------------------------------------------------
set "BASE="
if exist "%SystemRoot%\System32\Recovery\Winre.wim" set "BASE=%SystemRoot%\System32\Recovery\Winre.wim"
if not defined BASE if exist "%SystemDrive%\Recovery\WindowsRE\Winre.wim" set "BASE=%SystemDrive%\Recovery\WindowsRE\Winre.wim"
if not defined BASE (
  echo [!] Winre.wim не найден.
  echo     Варианты:
  echo       - включить среду восстановления:  reagentc /enable
  echo       - достать из установочного ISO:
  echo           dism /Mount-Wim /WimFile:D:\sources\install.wim /Index:1 /MountDir:C:\mnt /ReadOnly
  echo           copy C:\mnt\Windows\System32\Recovery\Winre.wim %SystemRoot%\System32\Recovery\
  echo           dism /Unmount-Wim /MountDir:C:\mnt /Discard
  exit /b 1
)
echo [1/6] База: %BASE%

rem --- 3. рантайм WebView2 ---------------------------------------------------
set "WV="
for /f "delims=" %%D in ('dir /b /ad /o-n "%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\1*" 2^>nul') do (
  if not defined WV set "WV=%ProgramFiles(x86)%\Microsoft\EdgeWebView\Application\%%D"
)
if not defined WV for /f "delims=" %%D in ('dir /b /ad /o-n "%ProgramFiles%\Microsoft\EdgeWebView\Application\1*" 2^>nul') do (
  if not defined WV set "WV=%ProgramFiles%\Microsoft\EdgeWebView\Application\%%D"
)
if not defined WV if exist "%SRC%\WebView2\msedgewebview2.exe" set "WV=%SRC%\WebView2"
if not defined WV (
  echo [!] Рантайм WebView2 не найден.
  echo     Поставьте Microsoft Edge WebView2 Runtime, либо положите его файлы
  echo     в "%SRC%\WebView2\" ^(нужен msedgewebview2.exe^).
  exit /b 1
)
echo [2/6] WebView2: %WV%

rem --- 4. монтируем базу и кладём нагрузку -----------------------------------
set "WORK=%OUT%\work"
set "MNT=%OUT%\mount"
if exist "%MNT%" dism /Unmount-Wim /MountDir:"%MNT%" /Discard >nul 2>&1
rmdir /s /q "%OUT%" >nul 2>&1
mkdir "%WORK%" 2>nul
mkdir "%MNT%" 2>nul

copy /y "%BASE%" "%WORK%\boot.wim" >nul || (echo [!] Не удалось скопировать базу & exit /b 1)
attrib -r -h -s "%WORK%\boot.wim"

echo [3/6] Монтирую образ…
dism /Mount-Wim /WimFile:"%WORK%\boot.wim" /Index:1 /MountDir:"%MNT%" || (
  echo [!] Монтирование не удалось. & exit /b 1
)

set "APP=%MNT%\Program Files\OwnBackup"
mkdir "%APP%" 2>nul
mkdir "%APP%\WebView2" 2>nul
copy /y "%SRC%\own-backup-gui.exe" "%APP%\" >nul
copy /y "%SRC%\own-backup-service.exe" "%APP%\" >nul
copy /y "%SRC%\own-backup-cli.exe" "%APP%\" >nul
echo [4/6] Копирую WebView2 ^(это около 250 МБ^)…
xcopy "%WV%\*" "%APP%\WebView2\" /e /i /y /q >nul

rem Запуск: WinPE грузится с X:, профиль браузера обязан лежать на записываемом
rem томе, песочница и GPU в WinPE недоступны.
> "%APP%\start-media.cmd" echo @echo off
>>"%APP%\start-media.cmd" echo wpeinit
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=X:\Program Files\OwnBackup\WebView2"
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--no-sandbox --disable-gpu --disable-gpu-compositing"
>>"%APP%\start-media.cmd" echo set "WEBVIEW2_USER_DATA_FOLDER=X:\Windows\Temp\OwnBackupWebView2"
>>"%APP%\start-media.cmd" echo md "%%WEBVIEW2_USER_DATA_FOLDER%%" 2^>nul
>>"%APP%\start-media.cmd" echo start "" "X:\Program Files\OwnBackup\own-backup-service.exe" --run
>>"%APP%\start-media.cmd" echo "X:\Program Files\OwnBackup\own-backup-gui.exe"
>>"%APP%\start-media.cmd" echo echo Интерфейс закрыт. Консоль остаётся для разбирательств.
>>"%APP%\start-media.cmd" echo cmd

rem Наш запуск вместо штатной оболочки среды восстановления.
> "%MNT%\Windows\System32\winpeshl.ini" echo [LaunchApps]
>>"%MNT%\Windows\System32\winpeshl.ini" echo "X:\Program Files\OwnBackup\start-media.cmd"

rem 32 МБ scratch по умолчанию движку браузера мало.
dism /Image:"%MNT%" /Set-ScratchSpace:512 >nul

echo [5/6] Сохраняю образ…
dism /Unmount-Wim /MountDir:"%MNT%" /Commit || (
  echo [!] Сохранение образа не удалось. & exit /b 1
)

rem --- 5. макет носителя (UEFI) ----------------------------------------------
set "MEDIA=%OUT%\media"
mkdir "%MEDIA%\EFI\BOOT" 2>nul
mkdir "%MEDIA%\EFI\Microsoft\Boot" 2>nul
mkdir "%MEDIA%\boot" 2>nul
mkdir "%MEDIA%\sources" 2>nul
copy /y "%SystemRoot%\Boot\EFI\bootmgfw.efi" "%MEDIA%\EFI\BOOT\BOOTX64.EFI" >nul
copy /y "%SystemRoot%\Boot\DVD\EFI\BCD" "%MEDIA%\EFI\Microsoft\Boot\BCD" >nul
copy /y "%SystemRoot%\Boot\DVD\EFI\boot.sdi" "%MEDIA%\boot\boot.sdi" >nul
move /y "%WORK%\boot.wim" "%MEDIA%\sources\boot.wim" >nul
rmdir /s /q "%WORK%" >nul 2>&1
rmdir /s /q "%MNT%" >nul 2>&1

echo [6/6] Готово.
echo.
echo Носитель собран здесь: %MEDIA%
dir /b "%MEDIA%"
echo.
echo Дальше — одно из двух:
echo   * флешка: разметить GPT + FAT32 ^(diskpart: clean / convert gpt /
echo     create partition primary / format fs=fat32 quick / assign^)
echo     и скопировать всё содержимое "%MEDIA%" в её корень;
echo   * виртуалка: скопировать папку "%MEDIA%" в
echo     \\wsl.localhost\Ubuntu\home\modulator\Development\own-backup\tmp\
echo     — из неё соберут загрузочный диск для VMware.
endlocal
