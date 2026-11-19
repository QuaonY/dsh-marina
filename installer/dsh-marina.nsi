; DSH Marina 安装包（NSIS 3）
;
; 编译：
;   makensis /DVERSION=0.1.2 installer\dsh-marina.nsi
; 产物：
;   installer\dsh-marina-setup-v<版本>-windows-x64.exe
;
; 设计取舍：
; - 默认装到 %LOCALAPPDATA%\Programs\DSH Marina，装和更新都不需要管理员权限；
;   用户仍可以在向导里改成任意目录（包括 Program Files，那就需要提权）。
; - 只写当前用户的东西：快捷方式、卸载项、可选的开机自启。
; - 卸载程序由 NSIS 生成；Marina 自己的「卸载」会优先调用它，避免两套卸载逻辑。

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "WinVer.nsh"

!ifndef VERSION
  !define VERSION "0.1.1"
!endif

!define APP_NAME "DSH Marina"
!define APP_EXE "dsh-marina.exe"
!define REG_KEY "Software\DSH Marina"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\DSHMarina"

Name "${APP_NAME} ${VERSION}"
OutFile "dsh-marina-setup-v${VERSION}-windows-x64.exe"
InstallDir "$LOCALAPPDATA\Programs\DSH Marina"
InstallDirRegKey HKCU "${REG_KEY}" "InstallDir"

BrandingText "${APP_NAME} ${VERSION}"

; 中文界面
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "立即启动 ${APP_NAME}"
!define MUI_FINISHPAGE_SHOWREADME ""
!define MUI_FINISHPAGE_SHOWREADME_NOTCHECKED

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; 已经装了旧版本时，安装前先让它正常退出。
Function .onInit
  ${IfNot} ${AtLeastWin10}
    MessageBox MB_ICONSTOP "DSH Marina 需要 Windows 10 或更高版本。"
    Abort
  ${EndIf}
  SetShellVarContext current
FunctionEnd

Function .onVerifyInstDir
  ; 目录必须可写；不可写时提示用户换一个。
  IfFileExists "$INSTDIR\*.*" writable_check
  Return
  writable_check:
    ClearErrors
    CreateDirectory "$INSTDIR\.write-test"
    ${If} ${Errors}
      MessageBox MB_ICONEXCLAMATION "这个目录无法写入，请选择别的目录，或换个有权限的位置。"
      Abort
    ${Else}
      RMDir "$INSTDIR\.write-test"
    ${EndIf}
FunctionEnd

Section "DSH Marina（必需）" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  SetOverwrite on

  ; 安装前先结束正在运行的 Marina，避免文件被占用。
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'
  Pop $0
  Sleep 400

  File "..\target\release\${APP_EXE}"
  File "..\LICENSE"
  File "..\README.md"
  File "..\README.zh-CN.md"

  ; 旧版更新时清掉多余的备份文件。
  Delete "$INSTDIR\${APP_EXE}.old"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKCU "${REG_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${REG_KEY}" "Version" "${VERSION}"

  ; 「应用和功能」条目
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "Quaon"
  WriteRegStr HKCU "${UNINST_KEY}" "HelpLink" "https://github.com/QuaonY/dsh-marina"
  WriteRegStr HKCU "${UNINST_KEY}" "URLInfoAbout" "https://github.com/QuaonY/dsh-marina"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "桌面快捷方式" SecDesktop
  SetShellVarContext current
  CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
SectionEnd

Section "开始菜单快捷方式" SecStartMenu
  SetShellVarContext current
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0
  CreateShortcut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

LangString DESC_SecMain ${LANG_SIMPCHINESE} "DSH Marina 主程序。"
LangString DESC_SecMain ${LANG_ENGLISH} "The DSH Marina application."
LangString DESC_SecDesktop ${LANG_SIMPCHINESE} "在桌面上放一个图标，双击就能打开。"
LangString DESC_SecDesktop ${LANG_ENGLISH} "Put a shortcut on the desktop."
LangString DESC_SecStartMenu ${LANG_SIMPCHINESE} "在开始菜单里放一个入口和卸载快捷方式。"
LangString DESC_SecStartMenu ${LANG_ENGLISH} "Add a Start menu entry and an uninstall shortcut."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} $(DESC_SecMain)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DESC_SecDesktop)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} $(DESC_SecStartMenu)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; 卸载前的确认：是否一并删除实例数据。
Var DeleteData

Function un.onInit
  SetShellVarContext current
  MessageBox MB_YESNO|MB_ICONQUESTION \
    "同时删除 Marina 的数据吗？$\r$\n$\r$\n包含所有实例、插件、会话和登录信息（通常在 %USERPROFILE%\.dsh-marina）。$\r$\n选「否」只删除程序，以后重装还能接着用。" \
    /SD IDNO IDYES delete_data
  StrCpy $DeleteData "0"
  Goto done
  delete_data:
    StrCpy $DeleteData "1"
  done:
FunctionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${APP_EXE}" /F'
  Pop $0
  Sleep 400

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${APP_EXE}.old"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\README.zh-CN.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$DESKTOP\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\${APP_NAME}"

  DeleteRegKey HKCU "${UNINST_KEY}"
  DeleteRegKey HKCU "${REG_KEY}"

  ; 开机自启如果还开着，一并清掉，免得指向一个不存在的程序。
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DSH Marina"

  ${If} $DeleteData == "1"
    SetShellVarContext current
    RMDir /r "$PROFILE\.dsh-marina"
  ${EndIf}
SectionEnd
