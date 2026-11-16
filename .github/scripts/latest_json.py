"""Собирает latest.json для автообновления из файлов .sig выпуска.

Раньше его собирал tauri-action: каждое задание сборки дописывало свою
платформу, поэтому задания шли по очереди. Теперь сборки идут параллельно и
только заливают установщики с подписями, а файл собирается здесь одним
заданием в конце (release.yml, задание publish).

Ключи платформ и файлы — те же, что писал tauri-action (сверено с
latest.json выпуска 0.6.2): подпись в latest.json — это содержимое .sig
как есть.

Запуск:
    python3 latest_json.py --version 0.7.0 --base-url URL --sigs DIR \
        --tag-message FILE --out latest.json --note note.txt

Проверка: python3 latest_json.py --self-test
"""

import argparse
import datetime
import json
import os
import re
import sys

# Платформа updater → шаблон имени файла (assetNamePattern из release.yml).
PLATFORMS = {
    "windows-x86_64": "Clipwire_{v}_windows_x64-setup.exe",
    "windows-x86_64-nsis": "Clipwire_{v}_windows_x64-setup.exe",
    "darwin-aarch64": "Clipwire_{v}_macos_universal.app.tar.gz",
    "darwin-x86_64": "Clipwire_{v}_macos_universal.app.tar.gz",
    "darwin-aarch64-app": "Clipwire_{v}_macos_universal.app.tar.gz",
    "darwin-x86_64-app": "Clipwire_{v}_macos_universal.app.tar.gz",
    "linux-x86_64": "Clipwire_{v}_linux_amd64.AppImage",
    "linux-x86_64-appimage": "Clipwire_{v}_linux_amd64.AppImage",
    "linux-x86_64-deb": "Clipwire_{v}_linux_amd64.deb",
}

_RECOMMENDED = re.compile(r"^\s*recommended\s*(?::\s*(.*?))?\s*$", re.IGNORECASE)


def parse_recommended(message):
    """Метка рекомендуемого выпуска из сообщения тега: строка «recommended»
    или «recommended: причина». None — обычный выпуск, "" — без причины."""
    for line in message.splitlines():
        m = _RECOMMENDED.match(line)
        if m:
            return (m.group(1) or "").strip()
    return None


def build(version, base_url, sigs, recommended, pub_date):
    """sigs: имя файла → содержимое .sig. Нет нужной подписи — ошибка:
    выпуск без платформы не публикуется (остаётся черновиком)."""
    platforms = {}
    missing = []
    for key, pattern in PLATFORMS.items():
        name = pattern.format(v=version)
        sig = sigs.get(name)
        if sig is None:
            missing.append(name)
            continue
        platforms[key] = {"signature": sig, "url": f"{base_url}/{name}"}
    if missing:
        raise SystemExit("нет подписей: " + ", ".join(sorted(set(missing))))
    doc = {"version": version, "notes": "", "pub_date": pub_date, "platforms": platforms}
    if recommended is not None:
        doc["recommended"] = recommended or True
    return doc


def read_sigs(directory):
    out = {}
    for f in os.listdir(directory):
        if f.endswith(".sig"):
            with open(os.path.join(directory, f), encoding="utf-8") as h:
                out[f[: -len(".sig")]] = h.read()
    return out


def self_test():
    assert parse_recommended("v0.7.0") is None
    assert parse_recommended("v0.7.0\n\nrecommended") == ""
    assert parse_recommended("v0.7.0\nRecommended: чинит синхронизацию  \nещё") == "чинит синхронизацию"
    assert parse_recommended("not recommended here") is None
    assert parse_recommended("recommendedx") is None
    sigs = {p.format(v="1.2.3"): "SIG:" + p for p in set(PLATFORMS.values())}
    doc = build("1.2.3", "https://x/v1.2.3", sigs, None, "T")
    assert "recommended" not in doc
    assert doc["platforms"]["linux-x86_64-deb"]["url"] == "https://x/v1.2.3/Clipwire_1.2.3_linux_amd64.deb"
    assert doc["platforms"]["darwin-x86_64"]["signature"].endswith("macos_universal.app.tar.gz")
    assert len(doc["platforms"]) == len(PLATFORMS)
    assert build("1.2.3", "u", sigs, "", "T")["recommended"] is True
    assert build("1.2.3", "u", sigs, "причина", "T")["recommended"] == "причина"
    del sigs["Clipwire_1.2.3_linux_amd64.deb"]
    try:
        build("1.2.3", "u", sigs, None, "T")
    except SystemExit as e:
        assert "linux_amd64.deb" in str(e)
    else:
        raise AssertionError("без подписи должно падать")
    print("ok")


def main():
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    p = argparse.ArgumentParser()
    p.add_argument("--version", required=True)
    p.add_argument("--base-url", required=True)
    p.add_argument("--sigs", required=True)
    p.add_argument("--tag-message", required=True, help="файл с сообщением тега (может быть пустым)")
    p.add_argument("--out", required=True)
    p.add_argument("--note", required=True, help="сюда пишется строка для описания релиза или ничего")
    a = p.parse_args()
    with open(a.tag_message, encoding="utf-8") as h:
        rec = parse_recommended(h.read())
    now = datetime.datetime.now(datetime.timezone.utc)
    pub_date = now.strftime("%Y-%m-%dT%H:%M:%S.") + f"{now.microsecond // 1000:03d}Z"
    doc = build(a.version, a.base_url, read_sigs(a.sigs), rec, pub_date)
    with open(a.out, "w", encoding="utf-8") as h:
        json.dump(doc, h, ensure_ascii=False, indent=2)
        h.write("\n")
    note = ""
    if rec is not None:
        note = "Рекомендуемое обновление." + (f" {rec}" if rec else "")
    with open(a.note, "w", encoding="utf-8") as h:
        h.write(note)


if __name__ == "__main__":
    main()
