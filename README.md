# Clipwire

Общий буфер обмена для Windows, Linux и macOS: копируешь на одной машине, вставляешь на другой.
Текст, картинки и файлы идут через свой relay-сервер, содержимое шифруется на устройствах (E2E).

## Состояние

Идёт этап 2: relay-сервер и Windows-клиент для текста, с E2E с самого начала.
Стек — Rust + [iroh](https://github.com/n0-computer/iroh), архитектура описана в
[docs/architecture.md](docs/architecture.md), обзор готовых решений — в
[docs/research.md](docs/research.md). Бриф и план по этапам — в [CLAUDE.md](CLAUDE.md).

Готово:
- `crates/clipwire-core` — модель элемента буфера, шифрование ключом группы
  (XChaCha20-Poly1305, ключи через HKDF-SHA256), коды сервера и приглашения, формат сообщений.
- `crates/clipwire-server` — сервер: встроенный iroh-relay с доступом по токену, группы
  устройств, раздача элементов подключённым устройствам и история с TTL в SQLite.
  Содержимое элементов сервер не видит.

В работе: агент для Windows (`clipwire-agent`).

## Сервер

Один контейнер. Наружу нужны HTTPS (nginx перед relay) и, по желанию, UDP-порт для прямых
QUIC-соединений: без него всё работает через relay, только чуть медленнее.

Готовый образ собирает GitHub Actions: `ghcr.io/erneywhite/clipwire-server:latest`
(последний `main`) и `:X.Y.Z` для релизов.

**Через панель управления с Docker (или просто `docker compose`):**

1. Docker → Compose: вставить [deploy/docker-compose.image.yml](deploy/docker-compose.image.yml),
   заменить `CLIPWIRE_PUBLIC_URL` на свой домен и запустить.
2. Создать обратный прокси на этот домен → `http://127.0.0.1:3340`, включить SSL и WebSocket.
   Если в панели нет переключателя WebSocket, в конфиг сайта вставляется блок из
   [deploy/nginx-clipwire.conf](deploy/nginx-clipwire.conf).
3. Открыть UDP 7842 в файрволе (необязательно).
4. По желанию закрыть служебную страницу iroh-relay на корне домена: в конфиг прокси
   добавить `location = /` с редиректом, пример в конце
   [deploy/nginx-clipwire.conf](deploy/nginx-clipwire.conf).
5. Взять из лога контейнера строку `код сервера: cwserver1-…` — с ней подключается первое
   устройство. То же печатает `clipwire-server info`.

Обновление: скачать свежий образ (pull) и пересоздать контейнер.

Проверить сервер снаружи: `cargo run -p clipwire-server --example probe -- cwserver1-…`.
Проба подключает два клиента через relay, гоняет между ними зашифрованный элемент и
показывает, каким путём шли пакеты.

**Собрать образ самому:** `docker compose up -d --build` в каталоге `deploy`
([deploy/docker-compose.yml](deploy/docker-compose.yml)).

Ключ сервера, токен relay и база лежат в томе Docker `clipwire-data`. Если удалить ключ, у сервера
сменится адрес, и устройства придётся подключать заново.

Переменные окружения:

| Переменная | По умолчанию | Что это |
|---|---|---|
| `CLIPWIRE_PUBLIC_URL` | обязательна | публичный адрес, например `https://clip.example.com` |
| `CLIPWIRE_DATA_DIR` | `/data` в образе | ключ, токен, база |
| `CLIPWIRE_RELAY_BIND` | `0.0.0.0:3340` | HTTP встроенного relay |
| `CLIPWIRE_QUIC_BIND` | `0.0.0.0:7842` | UDP для прямых соединений |
| `CLIPWIRE_RELAY_TOKEN` | генерируется | токен доступа к relay, если хочется задать свой |
| `CLIPWIRE_MAX_ITEM_BYTES` | `1048576` | предел одного зашифрованного элемента |
| `CLIPWIRE_HISTORY_LEN` | `50` | сколько последних элементов хранить на группу |
| `CLIPWIRE_TTL_HOURS` | `24` | сколько хранить элемент |
| `CLIPWIRE_SELF_RELAY_URL` | `http://127.0.0.1:<порт relay>` | как сервер сам подключается к своему relay |

## Сборка и тесты

Нужен Rust stable (проверено на 1.100), на Windows ещё MSVC Build Tools.

```
cargo test
```

Сквозные тесты в `crates/clipwire-server/tests/e2e.rs` поднимают настоящий сервер с relay на
127.0.0.1 и гоняют через него несколько клиентов.

## Лицензия

[AGPL-3.0](LICENSE). Пользоваться, менять и распространять можно, но изменённые версии,
включая сервер, к которому подключаются по сети, обязаны публиковаться с исходным кодом
под той же лицензией.

## Автор

Erney White
