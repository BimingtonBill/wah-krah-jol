@echo off
rem OpenSkyrim: walk into Riverwood and through four of its houses, no loading screens.
rem Mouse to look (click the window first), WASD move, Shift run, Space jump,
rem E open a load door, F fly, Esc release the mouse. Close the window to quit.
cd /d "%~dp0"
if not exist "target\release\engine.exe" (
  echo Building the engine first, this takes a few minutes...
  cargo build --release -p engine --bin engine || pause
)
start "" "target\release\engine.exe" --assets "%OPENSKYRIM_CONVERTED_DIR%" --demo riverwood --walk
