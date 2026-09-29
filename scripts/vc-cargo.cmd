@echo off
rem Runs a command inside the Visual Studio 2022 x64 developer
rem environment (vcvars64.bat), e.g.:
rem   scripts\vc-cargo.cmd cargo test --workspace
rem
rem The project targets x86_64-pc-windows-msvc. The MSVC linker and
rem libraries come from a VS2022 installation (Build Tools or full IDE)
rem located via vswhere. VS2026 is intentionally excluded: its VC
rem toolset on this machine lacks the x64 libraries (LNK1104 msvcrt.lib).
rem GNU/MinGW is not used: Smart App Control blocks its unsigned tools.

setlocal

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
    echo error: vswhere.exe not found at "%VSWHERE%" 1>&2
    exit /b 1
)

rem Latest VS2022 (17.x) instance with the x64 C++ build tools.
rem A temp file is used instead of FOR /F: the ')' of the version range
rem "[17.0,18.0)" would prematurely close the FOR ... IN (...) clause.
set "VS_PATH_FILE=%TEMP%\rsearch-vs-path-%RANDOM%.txt"
"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -version "[17.0,18.0)" -property installationPath >"%VS_PATH_FILE%"
set /p "VS_INSTALL=" <"%VS_PATH_FILE%"
del "%VS_PATH_FILE%" >nul 2>&1

if not defined VS_INSTALL (
    echo error: no Visual Studio 2022 installation with the C++ toolset found. 1>&2
    echo        Install Visual Studio Build Tools 2022 with the VCTools workload. 1>&2
    exit /b 1
)

set "VCVARS=%VS_INSTALL%\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
    echo error: vcvars64.bat not found under "%VS_INSTALL%" 1>&2
    exit /b 1
)

call "%VCVARS%" >nul
if errorlevel 1 (
    echo error: vcvars64.bat failed for "%VS_INSTALL%" 1>&2
    exit /b 1
)

rem Ensure cargo is reachable even when the caller's PATH is stale.
where cargo >nul 2>&1
if errorlevel 1 set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

if "%~1"=="" (
    echo usage: %~nx0 ^<command^> [args...] 1>&2
    exit /b 1
)
call %*
exit /b %errorlevel%
