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
  (XChaCha20-Poly1305, ключи через HKDF-SHA256), код приглашения, формат сообщений.

В работе: сервер (`clipwire-server`) и агент для Windows (`clipwire-agent`).

## Сборка

Нужен Rust stable (проверено на 1.100), на Windows ещё MSVC Build Tools.

```
cargo test
```

## Автор

Erney White
