; Хуки установщика NSIS (подключаются шаблоном Tauri через installerHooks).
;
; Подпись внизу окна (BrandingText) NSIS рисует прямо на разделительной черте.
; В тёмной теме Windows у подписи прозрачный фон, и черта проходит сквозь
; текст. Переопределить BrandingText отсюда нельзя (шаблон задаёт его позже),
; поэтому прячем сам элемент при открытии окна. Копирайт в свойствах файла
; остаётся.

!define MUI_CUSTOMFUNCTION_GUIINIT ClipwireHideBranding
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.ClipwireHideBranding

!macro CLIPWIRE_HIDE_BRANDING
  ; 1028 — текст подписи, 1256 — его копия, которую рисует MUI.
  GetDlgItem $0 $HWNDPARENT 1028
  ShowWindow $0 ${SW_HIDE}
  GetDlgItem $0 $HWNDPARENT 1256
  ShowWindow $0 ${SW_HIDE}
!macroend

Function ClipwireHideBranding
  !insertmacro CLIPWIRE_HIDE_BRANDING
FunctionEnd

Function un.ClipwireHideBranding
  !insertmacro CLIPWIRE_HIDE_BRANDING
FunctionEnd
