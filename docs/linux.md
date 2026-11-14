# Клиент для Linux

План этапа 4 для Linux и задание для работы в виртуалке. Проект собирается на Linux уже
сейчас: CI гоняет fmt, clippy и тесты на Ubuntu. Вся платформенная часть сидит за
`cfg(windows)` и `cfg(target_os = "macos")`, на Linux стоят заглушки (ищутся по
`cfg(not(any(windows, target_os = "macos")))` и `shortcut:` с «Linux»). Задача —
заменить их настоящей работой, не сломав Windows и macOS.

Образец — `docs/macos.md`: тот же порядок, тот же формат ручных проверок.

## Стенд

- VMware Workstation, Ubuntu 24.04 Desktop (GNOME). Обе сессии на одной машине: на экране
  входа шестерёнкой выбирается «Ubuntu» (Wayland) или «Ubuntu on Xorg».
- Сеть мостом: виртуалка в одной сети с Windows-ПК и маком, чтобы проверять и прямую
  передачу iroh, и relay.
- Общий буфер и перетаскивание между хостом и гостем в VMware выключены: иначе не видно,
  что передал Clipwire, а что VMware Tools.
- Зависимости для сборки (как в CI):

```bash
sudo apt-get install -y build-essential curl git pkg-config libssl-dev \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev \
  wl-clipboard xclip
```

  Rust через rustup, Node 22 или новее.

## Решения

- X11 и Wayland — оба. Сначала X11 (там всё возможно), потом Wayland с запасными
  вариантами. Сессию определять по `XDG_SESSION_TYPE` / `WAYLAND_DISPLAY`.
- Цель — GNOME (Ubuntu по умолчанию). KDE Plasma проверяется позже: у неё на Wayland есть
  `wlr-data-control`-подобный доступ к буферу, у GNOME нет.
- Пакеты: `.deb` и AppImage. Автообновление Tauri работает только для AppImage; `.deb`
  обновляется скачиванием нового пакета, это написать в README.
- Горячая клавиша по умолчанию — Ctrl+Shift+V. На Wayland глобальные клавиши у
  приложений не работают: там клавиша назначается в настройках GNOME на команду
  `clipwire --popup`. Такого ключа ещё нет: второй запуск должен передать его уже
  работающей копии (плагин single-instance) и открыть окно вставки. README объясняет,
  как назначить клавишу.

## Что сделать

Порядок важен: слежение за буфером — основа.

### 1. Буфер обмена (`crates/clipwire-agent/src/clipboard.rs`)

Сейчас `Watcher::start` на Linux возвращает «ещё не реализовано», запись умеет только текст.

- **X11:** модуль `x11` через `x11rb`: окно-невидимка, событие `XFixesSelectionNotify` на
  `CLIPBOARD` (без опроса). Чтение по `TARGETS`: `UTF8_STRING`, `text/html`, `text/rtf`
  (в `Content::Rich`), `image/png` (иначе другие форматы с переводом в PNG),
  `text/uri-list` и `x-special/gnome-copied-files` (файлы Nautilus) в `Content::Files`
  через `local_file_ref`. Большие данные идут через INCR — поддержать.
- **Запись на X11:** владеть `CLIPBOARD` и отвечать на запросы, пока буфер не перехватят.
  Для этого нужен живой поток с окном; при выходе из программы буфер пропадает (как у
  всех на X11, менеджер буфера GNOME его не сохраняет).
- **Wayland:** GNOME не даёт фоновому приложению ни следить за буфером, ни писать в него
  без фокуса. Варианты по порядку: `wlr-data-control` / `ext-data-control` (есть в KDE,
  Sway, Hyprland; в GNOME нет); на GNOME — XWayland-окно видит буфер, только пока
  активно X-приложение; честный путь для GNOME — маленькое расширение GNOME Shell,
  которое шлёт смены буфера по D-Bus. Сначала проверить, что реально работает на 24.04,
  и записать вывод в этот файл, потом решать.
- Приватное не трогать: тип `x-kde-passwordManagerHint` = `secret` (KeePassXC и другие).
- Защита от петель — как на других ОС (`loopguard.rs`).

Критерий: `cargo run -p clipwire-agent -- run` в сессии Xorg ловит копирование текста,
форматированного текста из Firefox, скриншота и файлов из Nautilus, а вставка из истории
вставляется в gedit, Nautilus и Firefox.

### 2. Программа-источник (`source_app.rs`)

Сделано (`source_app/linux.rs`):

- **Кто скопировал.** Владелец буфера X11 → процесс через расширение XRes
  (`LOCAL_CLIENT_PID`; точнее, чем активное окно: копировать можно и из фона) →
  `/proc/<pid>/exe`. У Flatpak путь внутри песочницы (`/app/…`) переводится в настоящий
  по `app-path` из `/.flatpak-info`. У скриптов (Python, Mono, Java, gjs…) берётся путь
  скрипта, а не интерпретатора: KeePass 2 так и остаётся `KeePass.exe`.
- **Имя и значок** из `.desktop`-файла. Файлы ищутся в `$XDG_DATA_HOME` и
  `$XDG_DATA_DIRS`, плюс экспорты Flatpak и `/var/lib/snapd/desktop`. Сопоставление по
  убыванию надёжности: id Flatpak-приложения, имя снапа, программа из `Exec`/`TryExec`
  (через `PATH`, ссылки раскрыты; `env …` и `flatpak run --command=` понимаются), обёртка в
  той же папке (`google-chrome` и `chrome`), `StartupWMClass`, имя файла и id.
  Имя локализуется по `Name[ru]` или через gettext (`X-Ubuntu-Gettext-Domain`, языковые
  пакеты Ubuntu). Значок ищется в теме значков сессии (`gsettings`, у KDE — `kdeglobals`),
  её родителях и `hicolor`; SVG рисуется `resvg` в 32×32. Проверено на GNOME 46: Firefox из
  snap, Терминал (`gnome-terminal-server`), Текстовый редактор, Файлы, Настройки.
- **Исключения** понимают имя файла, id `.desktop`-файла (`org.keepassxc.KeePassXC`) и имя
  программы. По умолчанию на Linux: `keepassxc`, `KeePass.exe`, `1password`, `bitwarden`,
  `secrets`, `seahorse`.
- **Wayland:** владелец буфера неизвестен (на GNOME буфером владеет сама оболочка через мост
  XWayland), исключения не работают. В настройках под списком программ это написано
  (`DeviceInfo.knows_source_app`).

### 3. Вставка в активное окно (`apps/desktop/src-tauri/src/popup/linux.rs`)

Сделано:

- **X11 (сессия Xorg).** Перед показом запоминается `_NET_ACTIVE_WINDOW`. После выбора окно
  вставки прячется; если менеджер окон сам не вернул фокус прежнему окну, ему уходит
  сообщение `_NET_ACTIVE_WINDOW` (источник «панель задач», с настоящим временем сервера).
  Когда прежнее окно снова активно и человек отпустил Shift/Ctrl/Alt/Super, через XTest
  нажимается Ctrl+V. В терминалах (по `WM_CLASS`: GNOME Terminal, Console, Konsole, Tilix,
  Alacritty, kitty, WezTerm, foot и др.) — Ctrl+Shift+V.
- **Wayland** (любой композитор, признак — `WAYLAND_DISPLAY`): нажимать клавиши в чужих окнах
  нельзя, где курсор — неизвестно. Окно вставки открывается по центру, Enter кладёт элемент
  в буфер, подсказка внизу — «в буфер». В настройках переключатель «Сразу вставлять» выключен
  с объяснением (`DeviceInfo.can_paste_into_active`).
- **`clipwire --popup`.** Второй запуск с этим ключом открывает окно вставки у уже запущенной
  копии (плагин single-instance, только в релизной сборке); первый запуск с ним — тоже.
  На Wayland глобальная клавиша программы работает только над окнами XWayland, поэтому в
  настройках подсказка: назначить сочетание в системе на `clipwire --popup`. Имя
  `clipwire` у исполняемого файла задаётся в сборке (пункт 7).

### 4. Оболочка Tauri

- Трей через AppIndicator. В чистом GNOME значков в трее нет: Ubuntu ставит расширение
  AppIndicator по умолчанию, на других GNOME README называет расширение.
- Монохромный значок под тему панели, как на маке (`icons/tray`).
- Окна: прозрачность и `backdrop-filter` в WebKitGTK — только с запасным вариантом
  (правило проекта). Заголовок — свой `TitleBar.svelte` с кнопками справа, как на Windows.
- Скрытие окон при демонстрации экрана на Linux недоступно — не делать.
- Автозапуск: `~/.config/autostart/clipwire.desktop` с `--background`.

### 5. Секреты (`secret.rs`)

Ключ в Secret Service (gnome-keyring, KWallet) через крейт `keyring` или `secret-service`,
файлы шифруются им — как сделано на macOS. Если Secret Service нет (голый оконный
менеджер), остаётся файл 0600 с предупреждением в логе. Старые файлы перенести.

### 6. Мелочи

- Звук копирования (`sound.rs`): тот же синтезированный щелчок через PulseAudio/PipeWire
  (`rodio` или `cpal`), без файлов.
- Отметка «скачано из интернета» на Linux не принята — заглушка остаётся, это не ошибка.
- Язык системы (`i18n.rs`): `LC_ALL` / `LC_MESSAGES` / `LANG`, у программ из меню они есть.
- Имя устройства: `/etc/hostname` или `hostnamectl --pretty`.
- Путь данных: `~/.local/share/Clipwire` (через `directories`, проверить).

### 7. Сборка и выпуск

- CI уже гоняет Linux. Добавить тесты фронтенда, если ещё нет.
- `release.yml`: задание `linux` после `macos` (общий `latest.json`), `tauri-action` с
  `.deb` и AppImage, `assetNamePattern: '[name]_[version]_linux_[arch][ext]'`, в
  `latest.json` — `linux-x86_64`.
- README и README.ru.md: установка на Linux, Wayland и его ограничения, расширение
  AppIndicator, горячая клавиша на Wayland.

## Как работать в виртуалке

- Ветка `linux` от `main`. В `main` — после проверки на Windows и зелёного CI на трёх ОС.
- Правила проекта — в `CLAUDE.md`: тексты через `tr()` / `t()` с английским переводом,
  новая логика с тестом, срезанные углы `shortcut:`, коммиты по-русски.
- Не ломать Windows и macOS: всё платформенное за `cfg(target_os = "linux")`. После
  каждого шага: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `node src/lib/*.test.ts` из `apps/desktop`.
- Запуск из исходников: `cd apps/desktop && npx tauri dev`. Для проверок отдельная папка
  данных: `CLIPWIRE_HOME=/tmp/cw-test`. В группу на чужом сервере без согласия не
  подключаться; для обмена с другими устройствами — свой сервер из `cargo run -p
  clipwire-server` или группа, на которую человек даст код.
- Всё, что требует человека у экрана, собирать в «ручные проверки» с точными шагами и
  ожидаемым результатом и отдавать пачкой. Каждую проверку — в обеих сессиях (Xorg и
  Wayland), если она от сессии зависит.

## Буфер на Wayland в GNOME 46: что проверено

Проверено 2026-11-14 на стенде (Ubuntu 24.04.5, GNOME Shell 46.0, сессия «Ubuntu» на
Wayland, Xwayland запущен как `:0`):

- **`wlr-data-control` / `ext-data-control` в mutter нет** — как и ожидалось. Чистый
  Wayland-путь для фоновой программы на GNOME закрыт.
- **Мост XWayland работает и без фокуса.** Mutter сам пересылает выбор между Wayland и X:
  - копирование Wayland-программой (`wl-copy`) сразу дало X-клиенту в фоне событие
    `XFixesSelectionNotify` на `CLIPBOARD` (зонд на `x11rb`, окно-невидимка без фокуса);
  - X-клиент в фоне читает содержимое (`xclip -o`) с правильными `TARGETS`;
  - запись X-клиентом в фоне (`xclip -i`) видна Wayland-стороне (`wl-paste`).
  Утверждение «XWayland видит буфер, только пока активно X-приложение» для GNOME 46 не
  подтвердилось.
- Проверено руками на настоящих Wayland-программах, зонд в фоне без фокуса получил
  событие и список форматов для каждой:

  | Откуда | Форматы |
  |---|---|
  | Текстовый редактор | `text/plain;charset=utf-8 UTF8_STRING` |
  | Firefox | `text/html`, `text/_moz_htmlcontext`, `UTF8_STRING`, `text/x-moz-url-priv`… |
  | Скриншот (PrtSc) | `image/png` |
  | Nautilus, файлы | только `x-special/gnome-copied-files` (без `text/uri-list`) |

Решение: **универсальный клиент, не только для GNOME.** Два слоя за одним `Watcher`:

1. **Wayland data-control** (`ext-data-control-v1`, если нет — `zwlr-data-control-v1`):
   KDE Plasma 6, Sway, Hyprland, labwc, COSMIC и другие на wlroots. Выбирается, если
   есть `WAYLAND_DISPLAY` и композитор объявляет один из протоколов.
2. **X11** (`x11rb`, `XFixesSelectionNotify`): Xorg любой оболочки и GNOME на Wayland через
   мост XWayland. Выбирается, если data-control нет, а `DISPLAY` есть.

Если нет ни того, ни другого (Wayland без XWayland и без data-control), слежения нет,
настройки это показывают. Расширение GNOME Shell не нужно.

Вставка и источник на Wayland ограничены в любом случае: синтезировать нажатия и узнать
активное окно приложению нельзя, поэтому окно вставки — запасной режим, исключения
программ не работают.

Для автотестов без экрана: `Xvfb` (X11) и `sway` в режиме headless (data-control).

## Ручные проверки (заполняется по ходу)

Стенд для пачки 2: свой сервер `clipwire-server` на 127.0.0.1, устройство 1 —
`clipwire-agent run` в сессии (`CLIPWIRE_HOME=/tmp/cw-test`), устройство 2 —
`clipwire-agent listen` (`/tmp/cw-test2`), печатает всё полученное.

| Что | Сессия | Как проверить | Результат |
|---|---|---|---|
| Смена буфера видна в фоне через XWayland | Wayland | Копирование в редакторе, Firefox, скриншот, Nautilus; зонд `XFixes` в фоне | ✅ все форматы пришли |
| Текст уходит в группу | Wayland | Ctrl+C в «Текстовом редакторе» | ✅ пришёл текст |
| Форматированный текст | Wayland | Ctrl+C куска страницы в Firefox | ✅ пришёл `Rich` |
| Скриншот | Wayland | PrtSc → снимок в буфер | ✅ пришла картинка 589×320 PNG |
| Файлы | Wayland | Ctrl+C на двух файлах в Nautilus | ✅ оба файла с размерами, залиты в хранилище |
| Пароль не уходит | Wayland | — (нужен KeePassXC, позже) | |
| Пришедший текст вставляется | Wayland | Ctrl+V в «Текстовом редакторе» после отправки с устройства 2 | ✅ вставилось |
| То же в Xorg | Xorg | Пачка 2 повторно в «Ubuntu on Xorg» | |
