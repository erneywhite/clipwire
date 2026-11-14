#!/bin/bash
# Подписать тестовую сборку Clipwire.app постоянным самоподписанным сертификатом,
# чтобы macOS не сбрасывала «Универсальный доступ» после каждой пересборки
# (у ad-hoc подписи требование подписи — хэш кода, он меняется с каждой сборкой).
#
#   apps/desktop/scripts/macos-dev-sign.sh [путь к .app]
#
# По умолчанию — target/debug/bundle/macos/Clipwire.app. При первом запуске
# создаёт сертификат «Clipwire Dev» в отдельной связке ключей
# ~/Library/Keychains/clipwire-dev.keychain-db: она не добавляется в список
# поиска и рабочую связку «login» не трогает. Это только для своих тестовых
# сборок; выпуски подписываются сертификатом из секретов CI (docs/macos.md, п. 7).
set -euo pipefail

NAME="Clipwire Dev"
KEYCHAIN="$HOME/Library/Keychains/clipwire-dev.keychain-db"
# Связка хранит только тестовый сертификат: пароль не секрет.
KC_PASS="clipwire-dev"
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
APP="${1:-$ROOT/target/debug/bundle/macos/Clipwire.app}"

[ -d "$APP" ] || { echo "нет $APP — сначала: npx tauri build --debug --bundles app" >&2; exit 1; }

if [ ! -f "$KEYCHAIN" ]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    cat > "$tmp/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
O = Erney White
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$tmp/cert.cnf" \
        -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null
    # -legacy: security import не читает p12 с шифрованием OpenSSL 3 по умолчанию.
    openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
        -name "$NAME" -out "$tmp/cert.p12" -passout pass:dev
    security create-keychain -p "$KC_PASS" "$KEYCHAIN"
    security set-keychain-settings "$KEYCHAIN" # без автоблокировки по времени
    security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
    security import "$tmp/cert.p12" -k "$KEYCHAIN" -P dev -T /usr/bin/codesign >/dev/null
    security set-key-partition-list -S apple-tool:,apple: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null
    echo "создан сертификат «$NAME» в $KEYCHAIN"
fi

security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
codesign --force --deep --keychain "$KEYCHAIN" -s "$NAME" "$APP"
codesign --verify --deep --strict "$APP"
codesign -dr - "$APP" 2>&1 | tail -1
