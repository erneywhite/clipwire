# Клиент для Linux

Как устроена поддержка Linux. Платформенный код сидит за `cfg(target_os = "linux")`, общий
код с Windows и macOS не разветвляется без нужды. Целевая среда — GNOME на Ubuntu 24.04
(Xorg и Wayland); работа в KDE Plasma и других оболочках описана ниже.

## Сборка и пакеты

Зависимости для сборки (как в CI):

```bash
sudo apt-get install -y build-essential curl git pkg-config libssl-dev \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev \
  wl-clipboard xclip
```

Нужны Rust (rustup) и Node 22 или новее. Запуск из исходников: `cd apps/desktop && npx tauri dev`;
для отдельной папки данных — `CLIPWIRE_HOME=/tmp/cw-test`. Пакеты собирает `npx tauri build`.

- `tauri.linux.conf.json`: `.deb` и AppImage, исполняемый файл `clipwire` (`mainBinaryName`),
  в `.deb` секция `utils` и рекомендация проигрывателя звука. Ярлык — свой шаблон
  `linux/clipwire.desktop` (английский `Comment` и русский `Comment[ru]`, ключевые слова,
  `StartupWMClass=clipwire`). Общий `longDescription` в `tauri.conf.json`.
- `release.yml`: задание `linux` на ubuntu-22.04 (старый glibc), параллельно с остальными,
  файлы `Clipwire_*_linux_amd64.deb` / `.AppImage`. `latest.json` (задание `publish`)
  получает `linux-x86_64` (AppImage) и `linux-x86_64-deb`. `APPIMAGE_EXTRACT_AND_RUN=1`:
  FUSE на раннерах нет.
- Обновления: AppImage обновляется тихо, как на других ОС. `.deb` плагин ставит через
  `pkexec dpkg -i` с паролем, поэтому только по кнопке (`updates::silent_install_possible`).
- Данные лежат в `~/.config/clipwire` (`directories::ProjectDirs::config_dir`, учитывает
  `XDG_CONFIG_HOME`).
- Автозапуск — плагин autostart: `~/.config/autostart/Clipwire.desktop` с `--background`, у
  AppImage — путь образа (`$APPIMAGE`).

## Буфер обмена

Код в `crates/clipwire-agent/src/clipboard/linux/`. Два слоя за одним `Watcher`, слой
выбирается один раз на запуск (принудительно — `CLIPWIRE_CLIPBOARD=x11|wayland`):

1. **Wayland data-control** (`wayland.rs`): `ext-data-control-v1`, если нет — `zwlr-data-control-v1`.
   Есть в KDE Plasma, Sway, Hyprland, labwc, COSMIC и других композиторах на wlroots.
   Выбирается, если есть `WAYLAND_DISPLAY` и композитор объявляет один из протоколов.
2. **X11** (`x11.rs`, `x11rb`): окно-невидимка и событие `XFixesSelectionNotify` на
   `CLIPBOARD`, без опроса. Работает в Xorg любой оболочки и в GNOME на Wayland через мост
   XWayland. Выбирается, если data-control нет, а `DISPLAY` есть.

Если нет ни того, ни другого (Wayland без XWayland и без data-control), слежения нет, окно
настроек это показывает.

Почему так. В mutter (GNOME 46) нет ни `wlr-data-control`, ни `ext-data-control`, и
фоновое Wayland-приложение не может следить за буфером. Зато мост XWayland работает и без
фокуса: копирование Wayland-программой даёт X-клиенту в фоне `XFixesSelectionNotify`, он
читает содержимое с правильными `TARGETS`, а его запись видна Wayland-программам. Расширение
GNOME Shell не нужно. Проверено на Ubuntu 24.04.5 с GNOME Shell 46 (сессия Wayland) на
текстовом редакторе, Firefox, скриншоте (PrtSc) и Nautilus.

Что читается и пишется:

- Текст (`UTF8_STRING`), форматированный текст (`text/html`, `text/rtf`, в `Content::Rich`),
  картинки (`image/png`, иначе другие форматы с переводом в PNG).
- Файлы: `text/uri-list` и `x-special/gnome-copied-files` (Nautilus кладёт только второй) в
  `Content::Files` через `local_file_ref`.
- Большие данные на X11 идут по INCR, на чтение и на запись.
- На X11 запись требует живого окна-владельца `CLIPBOARD`: при выходе из программы буфер
  пропадает, как у всех X11-программ (менеджер буфера GNOME его не сохраняет).
- Приватное не отправляется: тип `x-kde-passwordManagerHint` = `secret` (KeePassXC и другие).
- Защита от петель — как на других ОС (`loopguard.rs`).

Автотесты без экрана: `Xvfb` (X11), `sway` в режиме headless (data-control) и
`kwin_wayland --virtual` (KWin, `zwlr_data_control_manager_v1` v2). Живые тесты запускаются
так: `CLIPWIRE_CLIPBOARD=x11 xvfb-run cargo test -p clipwire-agent live -- --ignored --test-threads=1`.

## Программа-источник

Код в `crates/clipwire-agent/src/source_app/linux.rs`.

- **Кто скопировал.** Владелец буфера X11 → процесс через расширение XRes
  (`LOCAL_CLIENT_PID`; точнее, чем активное окно: копировать можно и из фона) →
  `/proc/<pid>/exe`. У Flatpak путь внутри песочницы (`/app/…`) переводится в настоящий по
  `app-path` из `/.flatpak-info`. У скриптов (Python, Mono, Java, gjs…) берётся путь скрипта,
  а не интерпретатора: KeePass 2 так и остаётся `KeePass.exe`.
- **Имя и значок** из `.desktop`-файла. Файлы ищутся в `$XDG_DATA_HOME` и `$XDG_DATA_DIRS`,
  плюс экспорты Flatpak и `/var/lib/snapd/desktop`. Сопоставление по убыванию надёжности: id
  Flatpak-приложения, имя снапа, программа из `Exec`/`TryExec` (через `PATH`, ссылки
  раскрыты; `env …` и `flatpak run --command=` понимаются), обёртка в той же папке
  (`google-chrome` и `chrome`), `StartupWMClass`, имя файла и id. Имя локализуется по
  `Name[ru]` или через gettext (`X-Ubuntu-Gettext-Domain`, языковые пакеты Ubuntu). Значок
  ищется в теме значков сессии (`gsettings`, у KDE — `kdeglobals`), её родителях и
  `hicolor`; SVG рисуется `resvg` в 32×32. Проверено на GNOME 46: Firefox из snap, Терминал
  (`gnome-terminal-server`), Текстовый редактор, Файлы, Настройки.
- **Исключения** понимают имя файла, id `.desktop`-файла (`org.keepassxc.KeePassXC`) и имя
  программы. По умолчанию на Linux: `keepassxc`, `KeePass.exe`, `1password`, `bitwarden`,
  `secrets`, `seahorse`.
- **Wayland:** владелец буфера неизвестен (на GNOME буфером владеет сама оболочка через мост
  XWayland), поэтому исключения не работают. В настройках под списком программ это написано
  (`DeviceInfo.knows_source_app`).

## Вставка в активное окно

Код в `apps/desktop/src-tauri/src/popup/`.

- **X11 (сессия Xorg).** Перед показом запоминается `_NET_ACTIVE_WINDOW`. После выбора окно
  вставки прячется; если менеджер окон сам не вернул фокус прежнему окну, ему уходит
  сообщение `_NET_ACTIVE_WINDOW` (источник «панель задач», с настоящим временем сервера).
  Когда прежнее окно снова активно и человек отпустил Shift/Ctrl/Alt/Super, через XTest
  нажимается Ctrl+V. В терминалах (по `WM_CLASS`: GNOME Terminal, Console, Konsole, Tilix,
  Alacritty, kitty, WezTerm, foot и др.) — Ctrl+Shift+V.
- **Wayland** (любой композитор, признак — `WAYLAND_DISPLAY`): нажимать клавиши в чужих окнах
  нельзя, где курсор — неизвестно. Окно вставки открывается по центру, Enter кладёт элемент в
  буфер, подсказка внизу — «в буфер». В настройках переключатель «Сразу вставлять» выключен
  с объяснением (`DeviceInfo.can_paste_into_active`).
- **Сама вставка на Wayland — по разрешению** (`popup/portal.rs`, настройка `portal_paste`,
  выключена по умолчанию). Портал `org.freedesktop.portal.RemoteDesktop`: сеанс только с
  клавиатурой, `PersistMode::ExplicitlyRevoked`, токен восстановления одноразовый и
  сохраняется после каждого запуска (`portal-token`, 0600). Включение в настройках вызывает
  диалог системы; отказ откатывает настройку. Ctrl+V — через `NotifyKeyboardKeysym`. Сеанс
  открывается при вставке и закрывается через минуту простоя: пока он открыт, GNOME
  показывает значок удалённого управления. Окно, куда вставлять, на Wayland неизвестно,
  поэтому всегда Ctrl+V (в терминалах не сработает).
- **Фокус окна вставки.** При показе из фона (по `--popup`) менеджер окон может не дать окну
  фокус. После показа окно, как только стало видимым, само просит `_NET_ACTIVE_WINDOW`
  (повтор каждые 250 мс, до 2 с).
- Проверено автоматически (Xvfb + openbox, релизная сборка): `--popup` → Enter вставляет в
  «Текстовый редактор» и в GNOME Terminal (Ctrl+Shift+V), по несколько прогонов подряд, в
  том числе с сочетанием Ctrl+Shift+V в настройках.

### Сочетание клавиш

Сочетание по умолчанию на Linux — Ctrl+Alt+V. Ctrl+Shift+V — вставка во всех терминалах;
глобальный захват этого сочетания сломал бы её. Если человек всё же выберет его, на время
вставки захват снимается. Тонкость: `global-hotkey` отправляет `UngrabKey` X-серверу только со
следующим запросом, ждущим ответа, поэтому после снятия регистрируется безобидное
`Ctrl+Alt+Shift+F24`, которое выталкивает отмену.

Глобальные клавиши самого приложения на Wayland работают только над окнами XWayland, поэтому
там сочетание регистрируется средствами оболочки, а в настройках подсказка: назначить
сочетание в системе на команду `clipwire --popup`.

- **`clipwire --popup`.** Второй запуск с этим ключом открывает окно вставки у уже запущенной
  копии (плагин single-instance, только в релизной сборке); первый запуск с ним тоже.
- **GNOME на Wayland** (`popup/gnome_shortcut.rs`): сочетание прописывается само. При
  регистрации Clipwire через `gsettings` заводит или правит своё в
  `org.gnome.settings-daemon.plugins.media-keys` → `custom-keybindings` (команда
  `<путь программы> --popup`, у AppImage — `$APPIMAGE`). Сочетание, заведённое руками (команда
  с `clipwire` и `--popup`), правится, а не дублируется. Проверено на живом профиле и тестом
  `gnome_live` в отдельном профиле dconf.
- **KDE Plasma** (`popup/kde_shortcut.rs`): сочетание регистрируется в службе сочетаний KDE
  (KGlobalAccel по D-Bus): компонент «Clipwire», действие `popup`, флаги
  `SetPresent | NoAutoloading`, нажатие приходит сигналом `globalShortcutPressed`. Работает в
  Plasma 5 и 6 без диалогов. Портал глобальных сочетаний в Plasma 5.27 сломан: служба портала
  1.18 не передаёт сочетания из `CreateSession`, а бэкенд KDE 5.27 ждёт их только там, ответ
  `BindShortcuts` пустой. Если сочетание уже занято (например, своим на `clipwire --popup`),
  KDE его не отдаёт, в логе предупреждение.
- **Hyprland, GNOME 48+ и другие с порталом GlobalShortcuts** (`popup/global_shortcuts.rs`,
  ashpd): `BindShortcuts` с предпочтительным сочетанием в записи XDG (`CTRL+ALT+v`), нажатия
  приходят сигналом `Activated`. На живой оболочке не проверялось.
- **Sway** этого не умеет: сочетание задаётся в его конфиге
  (`bindsym … exec clipwire --popup`).

### Место окна на Wayland

Где курсор, программе на Wayland не видно, а GNOME ещё и не даёт нативному окну выбрать
место (layer-shell в mutter нет). Поэтому в GNOME на Wayland Clipwire выставляет себе
`GDK_BACKEND=x11` до запуска GTK (`popup::linux::use_xwayland`, отключается
`CLIPWIRE_NATIVE_WAYLAND=1`), и окно вставки встаёт справа сверху главного монитора, под
значком в трее (`Placement::TopRight`). Фокус берётся через `_NET_ACTIVE_WINDOW`, как на X11.
Минус: при дробном масштабе (125 %, 150 %) окна XWayland в GNOME 46 чуть размыты.

В KDE Plasma окна тоже идут через XWayland, и окно вставки встаёт в угол у панели со стороны
трея (`Placement::Tray`, `tray_corner`): панель — полоса, на которую рабочая область
(`_NET_WORKAREA`) меньше экрана; у верхней или нижней панели — справа, у боковой — внизу. В
остальных Wayland-оболочках окно открывается по центру (`Placement::Center`).

layer-shell пробовали (`zwlr_layer_shell_v1` через `libgtk-layer-shell`, `dlopen`): слой
создаётся и получает размер, но окно Tauri в нём не рисует ни кадра (GTK не прикрепляет буфер
после `configure`), а KDE кладёт слой поверх своей панели. От этого отказались.

## Оболочка Tauri

- **Трей** через AppIndicator (StatusNotifierItem, `libayatana-appindicator3`). В чистом GNOME
  значков в трее нет: Ubuntu ставит расширение AppIndicator по умолчанию, для остальных
  дистрибутивов расширение нужно поставить самому. Меню открывается любым кликом, двойного
  клика нет, «Настройки» есть в меню.
- **Значок однотонный**, формы те же, что на macOS (`mac-on/dim/off.png`), перекрашиваются при
  загрузке: белый на тёмной панели (GNOME и большинство оболочек), чёрный на светлой схеме
  KDE (`ColorScheme` в `kdeglobals` без «Dark»).
- **Окна:** прозрачность и `backdrop-filter` не используются нигде. Заголовок — свой
  `TitleBar.svelte` с кнопками справа, как на Windows.
- **Скрытие окон при демонстрации экрана** на Linux не работает, переключатель в
  «Приватности» скрыт.

## Секреты

Код в `secret.rs`. Случайный ключ Clipwire лежит в Secret Service (крейт `secret-service`,
блокирующий API на своём потоке async-io; атрибуты `application=io.github.erneywhite.clipwire`,
`type=state-key`, подпись «Clipwire»). Файлы шифруются им в том же формате, что на macOS
(`CWK1`, XChaCha20-Poly1305, общий модуль `secret::sealed`). Шифрует приложение
(`enable_keychain` в `lib.rs`); консольный агент и тесты пишут открыто. Старые открытые файлы
при чтении переписываются зашифрованными. Запись ключа видна командой
`secret-tool search application io.github.erneywhite.clipwire`.

- Запертая связка (автовход в Ubuntu оставляет её запертой): открывается с вопросом пароля.
  Таймаут 8 с действует только на подключение и поиск ключа, окно пароля ждёт сколько угодно.
- Хранилища нет на шине или оно не ответило за 8 с: новые секреты пишутся открыто в файл 0600
  с предупреждением, дальше в этом запуске хранилище не пробуется.
- Сбои (`secret::imp::Failure`): отказ открыть связку, молчание службы, прочая ошибка —
  повтор не чаще раза в минуту (полминуты для прочих), запись секрета возвращает понятную
  ошибку, а не открытый текст. Правила — `may_try`, `retry_after`, `plain_allowed`, тесты
  `keystore_retry_rules`, `keystore_error_is_found_and_readable`.
- При запуске история без ключа не открывается: программа ждёт и спрашивает снова.
  Синхронизация, не запустившаяся по той же причине, сама перезапускается, когда ключ стал
  доступен (`retry_service_later`).
- Проверка Secret Service — тест `secret_service_roundtrip`.

## Прочее

- **Звук копирования** (`sound.rs`): тот же `sounds/copy.wav`, один раз пишется в
  `$XDG_RUNTIME_DIR`, играет системный проигрыватель: `pw-play` (PipeWire), `paplay`
  (PulseAudio) или `aplay -q` (ALSA), первый найденный в `PATH`. Свой аудиопоток в программе
  держал бы звуковую карту ради щелчка раз в несколько минут. Новый звук обрывает ещё
  играющий. Если нет ни одного, звука нет и в логе строка.
- **Отметка «скачано из интернета»** на Linux не ставится: функция-заглушка, это не ошибка.
- **Язык системы** (`i18n.rs`): `LC_ALL` / `LC_MESSAGES` / `LANG`, у программ из меню они есть.
- **Имя устройства** по умолчанию: `PRETTY_HOSTNAME` из `/etc/machine-info` (его задают
  «Настройки» → «О системе» в GNOME и `hostnamectl --pretty`), иначе `/etc/hostname`.
  Переменной `HOSTNAME` у программ из меню нет.
