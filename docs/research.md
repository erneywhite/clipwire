# Шаг 0. Готовые решения

Проверено 12.11.2026. Источники: README, GitHub API, issues. Сами программы я не запускал,
так что всё про поведение известно из документации и отчётов пользователей.

## Что от решения требуется

1. Windows, Linux и macOS.
2. Текст, картинки и файлы.
3. Работа через интернет, через свой сервер.
4. E2E-шифрование: сервер видит только шифротекст.
5. Ленивая передача файлов: при копировании уходят метаданные, содержимое качается при вставке.

## Сводка

| Проект | Жив | ОС | Файлы | Через интернет, свой сервер | E2E | Главная проблема |
|---|---|---|---|---|---|---|
| [UniClipboard](https://github.com/UniClipboard/UniClipboard) | да, v1.6.2 от 12.11.2026 | Win, mac, Linux, мобильные отдельно | да, крупные по запросу | да: P2P с обходом NAT, иначе relay, свой relay в Docker | да | один разработчик, 183 открытых issue, на Linux сыро |
| [CrossPaste](https://github.com/CrossPaste/crosspaste-desktop) | да, 2.3.1 от 30.10.2026 | Win, mac, Linux, Android, iOS | да | нет, только локалка через mDNS | да | нет сервера, JVM ест около 500 МБ RAM |
| [ClipCascade](https://github.com/Sathvik-Rao/ClipCascade) | да, v3.5.1 от 08.11.2026 | Win, mac, Linux, Android | да, но сразу при копировании и в памяти | да, свой сервер (Java, Docker) | заявлено, шифр в README не назван | лимит 1 МиБ по умолчанию, ключ из пароля, клиенты на Python ломаются |
| [KDE Connect](https://github.com/KDE/kdeconnect-kde) / [GSConnect](https://github.com/GSConnect/gnome-shell-extension-gsconnect) | да | Linux, Windows, macOS только nightly | отдельной отправкой, не через буфер | нет, только локалка или VPN | TLS между устройствами | не relay, на Win и mac буфер работает плохо |
| [Deskflow](https://github.com/deskflow/deskflow) | да, 1.27.0 | Win, mac, Linux | не подтверждено | нет | TLS | это шаринг клавиатуры и мыши, буфер к нему в нагрузку |
| [DecentPaste](https://github.com/decentpaste/decentpaste) | неясно | Win, mac, Linux, Android | нет, в планах | нет, relay в планах | да, PIN-pairing | только текст |
| [cliprelay](https://github.com/Swatto86/cliprelay) | неясно | только Windows | до 50 МиБ, сразу | да, свой relay на WebSocket | да | только Windows, но протокол хороший для примера |
| [uniclip](https://github.com/quackduck/uniclip) | нет, последний коммит 03.2024 | Win, mac, Linux | нет | нет | да | заброшен, только текст |

Мимо требований по сути: LocalSend (передача файлов, а не буфер), Ditto (только Windows,
синхронизация по локалке), CopyQ (менеджер буфера, синхронизации через сервер нет),
ClipShare (нет E2E и relay).

## Главные кандидаты подробнее

### UniClipboard

На бумаге закрывает почти весь бриф. Это первое, что надо пощупать руками.

- Стек: GUI на Go/Wails + React, ядро синхронизации на Rust ([Engine](https://github.com/UniClipboard/Engine),
  Apache-2.0), сеть на iroh (QUIC, P2P с обходом NAT).
- Шифрование: XChaCha20-Poly1305, ключи пространства выводятся из парольной фразы через
  Argon2id (128 МиБ, 3 итерации). Устройство добавляется по короткому коду приглашения
  плюс парольная фраза.
- Свой relay: отдельный репозиторий [relay](https://github.com/UniClipboard/relay)
  (AGPL-3.0). Это Docker Compose из двух контейнеров: шлюз на Go со статическим токеном и
  обычный `iroh-relay`. TLS снаружи через реверс-прокси, то есть встаёт за любой обратный прокси.
- Файлы: сейчас мелкие передаются сразу, крупные по запросу (порог по размеру). В
  [#1932](https://github.com/UniClipboard/UniClipboard/issues/1932) обсуждают режим
  «только при вставке» с нативной отложенной вставкой на каждой ОС. Ровно наша идея.
- Есть история с полнотекстовым поиском. Про исключение приложений в README ничего нет.

Риски:
- Почти все коммиты (около 4000 за полтора года) от одного человека. Если он бросит
  проект, поддерживать будет некому.
- 183 открытых issue. На Linux есть свежие баги: WebKit грузит CPU на 100%
  ([#2009](https://github.com/UniClipboard/UniClipboard/issues/2009)), блокирующий опрос
  буфера X11 ([#1097](https://github.com/UniClipboard/UniClipboard/issues/1097)).
- Есть жалоба, что macOS и Windows не спариваются через свой relay
  ([#1692](https://github.com/UniClipboard/UniClipboard/issues/1692)).
- Wayland в README не упомянут, работает ли — неизвестно.
- Лицензия AGPL-3.0: форк остаётся под AGPL. Для личного использования это не мешает.

### CrossPaste

Самый вылизанный по криптографии и pairing (SPAKE2 по PIN), активно чинят Wayland
(data-control, портал RemoteDesktop). Но архитектура «только локалка, без сервера»: в
issues и роадмапе нет ни relay, ни работы через интернет, а pairing без mDNS невозможен
([#4855](https://github.com/CrossPaste/crosspaste-desktop/issues/4855)). Прикручивать
сервер — переделка ядра под AGPL. Через WireGuard, скорее всего, заработает, но это не
проверено.

### ClipCascade

Модель как у нас (свой сервер, клиенты по WebSocket), но реализация слабее брифа:
- файлы уходят сразу при копировании и держатся в памяти, на больших файлах RAM улетала
  за 54 ГБ ([#165](https://github.com/Sathvik-Rao/ClipCascade/issues/165));
- лимит сообщения на сервере по умолчанию 1 МиБ (`CC_MAX_MESSAGE_SIZE_IN_MiB`);
- ключ выводится из пароля пользователя, pairing нет; алгоритм шифрования в README не назван;
- десктопные клиенты на Python + PyInstaller, свежие падения на macOS и поломки на Arch и Fedora.

## Что можно взять, если всё-таки писать своё

- **Ленивая вставка файлов в Windows.** `CFSTR_FILEDESCRIPTORW` с метаданными и
  `CFSTR_FILECONTENTS`, содержимое отдаётся только когда приложение вызывает
  `IDataObject::GetData` (тот же механизм, что у перетаскивания виртуальных файлов,
  [MS Docs](https://learn.microsoft.com/en-us/windows/win32/shell/datascenarios)). На macOS
  и Linux настоящий аналог не подтверждён, там нужен запасной путь из брифа: скачать в
  папку и положить в буфер путь.
- **Wayland.** Протокол `ext-data-control-v1` (раньше `wlr-data-control`) работает на
  wlroots и KDE. GNOME его внешним клиентам не даёт, нужно расширение
  ([mutter#524](https://gitlab.gnome.org/GNOME/mutter/-/issues/524)).
- **Relay без доступа к содержимому.** Шифрование XChaCha20-Poly1305 на клиенте, ключ
  передаётся через QR или ссылку и на сервер не попадает (так сделано в
  [Echo](https://dev.to/iamjephter/building-a-blind-relay-in-rust-with-tauri-at-the-edge-57gp)),
  счётчики против повторов и файлы чанками (как в cliprelay).
- **Транспорт.** Готовый `iroh` + `iroh-relay` даёт P2P с обходом NAT и relay в одном
  флаконе, писать свой WebSocket-relay может оказаться незачем.
- **Pairing.** SPAKE2 по короткому PIN (CrossPaste) или X25519 + PIN (DecentPaste).

## Решение (12.11.2026)

Пишем свой Clipwire. У других берём идеи: архитектуру, протоколы, UX-находки. Код из
AGPL- и GPL-проектов (UniClipboard, его relay, CrossPaste, ClipCascade) не копируем.
Библиотеки с пермиссивной лицензией, например iroh (Apache-2.0 / MIT), подключать
зависимостями можно.

Ниже исходный вывод обзора, оставлен для истории.

## Вывод обзора

Писать с нуля пока рано. UniClipboard уже делает почти всё из брифа: три ОС, файлы,
свой relay в Docker, E2E, работу в 1.6.2 над ленивой передачей. Его слабые места —
один разработчик и сырость на Linux. Это проверяется тестом, а не чтением.

Предлагаю так:
1. Поставить UniClipboard 1.6.2 на две-три свои машины.
2. Поднять его relay на одном из серверов (Docker Compose за обратным прокси).
3. Прогнать сценарии: текст и картинка через интернет, файл на 2 ГБ (сразу или по
   запросу), pairing Windows ↔ Linux ↔ macOS, твой Linux DE (особенно если это GNOME на
   Wayland), потребление памяти в фоне.
4. По итогам решить:
   - всё устраивает — просто пользуемся, Clipwire не нужен;
   - мелкие дыры — шлём PR в UniClipboard или держим свой форк;
   - серьёзные проблемы (стабильность, Linux, архитектура) — пишем Clipwire, взяв из
     этого отчёта iroh, схему шифрования и Windows-механизм отложенной вставки.
