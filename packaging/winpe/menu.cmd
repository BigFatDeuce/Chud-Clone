@echo off
rem Chud Clone - WinPE rescue key menu (started by startnet.cmd).
rem Every operation goes through chud-clone-cli.exe: same checks as the application.
rem Language: lang.txt ("en" or "fr", written by make-winpe.ps1); option 9 toggles it.
rem This file stays pure ASCII: cmd misreads batch files containing UTF-8 characters.
rem The messages are in menu-en.txt / menu-fr.txt (UTF-8), loaded with chcp 65001.
chcp 65001 >nul
setlocal EnableExtensions EnableDelayedExpansion
set "HOME=%~dp0"
set "CLI=%HOME%chud-clone-cli.exe"
set "LNG=en"
if exist "%HOME%lang.txt" set /p LNG=<"%HOME%lang.txt"
if /i not "!LNG!"=="fr" set "LNG=en"

rem Reports folder: on the key when it is writable, otherwise in memory (lost at shutdown).
set "REP=X:\ChudClone-reports"
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do (
  if exist "%%d:\ChudClone-rescue.txt" (
    if not exist "%%d:\ChudClone-reports\" mkdir "%%d:\ChudClone-reports" 2>nul
    if exist "%%d:\ChudClone-reports\" set "REP=%%d:\ChudClone-reports"
  )
)
if not exist "%REP%\" mkdir "%REP%" 2>nul

:menu
call :messages
set "CHUD_LANG=!LNG!"
cls
echo.
echo   ==========================================================
echo     CHUD CLONE  -  !T_TITLE!
echo   ==========================================================
echo     !T_INTRO1!
echo     !T_INTRO2!
echo.
echo     !T_REPORTS! %REP%
echo.
echo     1. !T_1!
echo     2. !T_2!
echo     3. !T_3!
echo     4. !T_4!
echo     5. !T_5!
echo     6. !T_6!
echo     7. !T_7!
echo     8. !T_8!
echo     9. !T_9!
echo.
choice /c 123456789 /n /m "   !T_CHOICE! "
goto choice%errorlevel%

:choice0
goto menu

:choice1
"%CLI%" list
goto pause

:choice2
call :disk N "!T_ASK_INSPECT!"
if errorlevel 1 goto menu
"%CLI%" inspect disk:!N!
goto pause

:choice3
call :disk N "!T_ASK_DISK!"
if errorlevel 1 goto menu
"%CLI%" smart disk:!N!
goto pause

:choice4
call :pair
if errorlevel 1 goto menu
"%CLI%" plan disk:!S! disk:!D!
goto pause

:choice5
call :pair
if errorlevel 1 goto menu
"%CLI%" plan disk:!S! disk:!D!
if errorlevel 1 (
  echo.
  echo   !T_BLOCKED!
  goto pause
)
echo.
echo   ----------------------------------------------------------
echo   !T_WARN1! !D! !T_WARN1B!
echo   !T_WARN2! !S!!T_WARN2B!
echo.
echo   !T_WARN3!
echo   !T_WARN4!
echo   !T_WARN5!
echo   ----------------------------------------------------------
set "OK="
set /p "OK=  !T_CONFIRM! !T_WORD! : "
if /i not "!OK!"=="!T_WORD!" (
  echo   !T_ABORTED!
  goto pause
)
"%CLI%" clone disk:!S! disk:!D! --confirm-erase --allow-unvalidated-physical-write --reports "%REP%"
echo.
echo   !T_SAVED! %REP%
goto pause

:choice6
echo   !T_EXIT!
cmd /k
goto menu

:choice7
wpeutil reboot
goto menu

:choice8
wpeutil shutdown
goto menu

:choice9
if "!LNG!"=="fr" (set "LNG=en") else (set "LNG=fr")
goto menu

:pause
echo.
pause
goto menu

rem --- Disk number input (digits only) ------------------------------------------
:disk
set "%~1="
set /p "%~1=  %~2 : "
echo(!%~1!| findstr /r "^[0-9][0-9]*$" >nul || (
  echo   !T_BADNUM!
  pause
  exit /b 1
)
exit /b 0

:pair
"%CLI%" list
echo.
call :disk S "!T_ASK_SRC!"
if errorlevel 1 exit /b 1
call :disk D "!T_ASK_DST!"
if errorlevel 1 exit /b 1
if "!S!"=="!D!" (
  echo   !T_SAME!
  pause
  exit /b 1
)
exit /b 0

rem --- Messages (menu-<lang>.txt: KEY=text lines) -------------------------------------
:messages
for /f "usebackq eol=# tokens=1* delims==" %%a in ("%HOME%menu-!LNG!.txt") do set "%%a=%%b"
exit /b 0
