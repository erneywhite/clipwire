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

X11: активное окно (`_NET_ACTIVE_WINDOW`) → `_NET_WM_PID` → `/proc/<pid>/exe`; имя и иконка
из `.desktop`-файла по `WM_CLASS` (`/usr/share/applications`, `~/.local/share/applications`,
Flatpak и Snap). Исключения в настройках — по имени `.desktop` и имени файла программы.
Wayland: источник неизвестен, исключения не работают — показать это в настройках.

### 3. Вставка в активное окно (`apps/desktop/src-tauri/src/popup.rs`)

- X11: запомнить активное окно, после выбора вернуть фокус (`_NET_ACTIVE_WINDOW`
  client message) и послать Ctrl+V через XTest.
- Wayland: синтезировать нажатия нельзя. Запасной режим из решений проекта: окно по
  центру, выбор кладёт в буфер, человек вставляет сам.

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

## Ручные проверки (заполняется по ходу)

| Что | Сессия | Как проверить | Результат |
|---|---|---|---|
| | | | |
