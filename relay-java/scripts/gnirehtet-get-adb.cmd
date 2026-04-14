@echo off
setlocal

set "SCRIPT_DIR=%~dp0"
set "ZIP_URL=https://dl.google.com/android/repository/platform-tools-latest-windows.zip"
set "ZIP_PATH=%TEMP%\platform-tools-latest-windows.zip"
set "EXTRACT_DIR=%TEMP%\gnirehtet-platform-tools-%RANDOM%%RANDOM%"

if exist "%SCRIPT_DIR%adb.exe" if exist "%SCRIPT_DIR%AdbWinApi.dll" if exist "%SCRIPT_DIR%AdbWinUsbApi.dll" (
    echo Android Platform-Tools already exist next to Gnirehtet.
    echo You can now run gnirehtet-run.cmd.
    exit /b 0
)

echo Downloading Android Platform-Tools from Google...
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '%ZIP_URL%' -OutFile '%ZIP_PATH%'"
if errorlevel 1 goto :fail

echo Extracting Android Platform-Tools...
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ProgressPreference='SilentlyContinue'; Expand-Archive -LiteralPath '%ZIP_PATH%' -DestinationPath '%EXTRACT_DIR%' -Force"
if errorlevel 1 goto :fail

if not exist "%EXTRACT_DIR%\platform-tools\adb.exe" goto :fail

copy /Y "%EXTRACT_DIR%\platform-tools\adb.exe" "%SCRIPT_DIR%adb.exe" >nul
copy /Y "%EXTRACT_DIR%\platform-tools\AdbWinApi.dll" "%SCRIPT_DIR%AdbWinApi.dll" >nul
copy /Y "%EXTRACT_DIR%\platform-tools\AdbWinUsbApi.dll" "%SCRIPT_DIR%AdbWinUsbApi.dll" >nul

if not exist "%SCRIPT_DIR%adb.exe" goto :fail
if not exist "%SCRIPT_DIR%AdbWinApi.dll" goto :fail
if not exist "%SCRIPT_DIR%AdbWinUsbApi.dll" goto :fail

del /Q "%ZIP_PATH%" >nul 2>&1
rmdir /S /Q "%EXTRACT_DIR%" >nul 2>&1

echo Android Platform-Tools are ready.
echo You can now run gnirehtet-run.cmd.
exit /b 0

:fail
del /Q "%ZIP_PATH%" >nul 2>&1
rmdir /S /Q "%EXTRACT_DIR%" >nul 2>&1
echo Failed to download or extract Android Platform-Tools.
echo Download them manually from:
echo %ZIP_URL%
exit /b 1
