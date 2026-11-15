; NSIS hooks (bundle.windows.nsis.installerHooks).

!macro NSIS_HOOK_POSTUNINSTALL
  ; The app registers its toast identity and URL scheme at runtime
  ; (notify::register_aumid / register_url_scheme); the stock uninstaller
  ; doesn't know about those keys or the icon it drops.
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\com.pcg.mail"
  DeleteRegKey HKCU "Software\Classes\pcgmail"
  Delete "$PROFILE\.pcgmail\notify-icon.png"
  RMDir "$PROFILE\.pcgmail"
!macroend
