; Хуки установщика NSIS (подключаются шаблоном Tauri через installerHooks).
;
; Подпись внизу окна (BrandingText) NSIS рисует прямо на разделительной черте.
; В тёмной теме Windows у подписи прозрачный фон, и черта проходит сквозь
; текст. Переопределить BrandingText отсюда нельзя (шаблон задаёт его позже),
; а просто спрятать элемент мало: страницы приветствия и завершения MUI сами
; прячут подпись и показывают её снова при переходе дальше. Поэтому стираем
; её текст при открытии окна, его MUI больше не трогает. Копирайт в свойствах
; файла остаётся.

!define MUI_CUSTOMFUNCTION_GUIINIT ClipwireClearBranding
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.ClipwireClearBranding

!macro CLIPWIRE_CLEAR_BRANDING
  ; 1028 — подпись NSIS, 1256 — её копия, которую рисует MUI.
  GetDlgItem $0 $HWNDPARENT 1028
  SendMessage $0 ${WM_SETTEXT} 0 "STR:"
  GetDlgItem $0 $HWNDPARENT 1256
  SendMessage $0 ${WM_SETTEXT} 0 "STR:"
!macroend

Function ClipwireClearBranding
  !insertmacro CLIPWIRE_CLEAR_BRANDING
FunctionEnd

Function un.ClipwireClearBranding
  !insertmacro CLIPWIRE_CLEAR_BRANDING
FunctionEnd

; Пункт «Отправить в Clipwire» писала в реестр пользователя версия 0.1.2.
; Новые версии убирают его при запуске (send_to.rs), а при удалении программы
; его убирает установщик, чтобы в меню Проводника не осталось ссылки в никуда.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey HKCU "Software\Classes\*\shell\Clipwire.Send"
  DeleteRegKey HKCU "Software\Classes\Directory\shell\Clipwire.Send"
!macroend
