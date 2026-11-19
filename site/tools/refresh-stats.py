#!/usr/bin/env python3
"""Обновляет stats.json сайта: звёзды на GitHub, загрузки по системам и последний выпуск.

Запускается по расписанию на сервере (Cron в панели), раз в час:

    python3 refresh-stats.py /путь/к/корню/сайта/stats.json

Сайт сначала читает этот файл со своего домена, поэтому у посетителей нет лимита
GitHub API в 60 запросов в час, и даже первый заход видит свежие цифры.

Правила те же, что в src/lib/stats.ts:
- загрузками считаются только установщики (.exe, .dmg, .deb, .AppImage), без подписей,
  архивов автообновления и latest.json;
- загрузки по каждой ОС не уменьшаются: берётся наибольшее из нового ответа и прошлого файла;
- если GitHub не ответил совсем, прошлый файл не трогаем.

Только стандартная библиотека Python 3.8+. GITHUB_TOKEN в окружении необязателен.
Проверка правил без сети: python3 refresh-stats.py --self-test
"""
import json
import os
import re
import sys
import tempfile
import time
import urllib.request

REPO = 'erneywhite/clipwire'
OSES = ('windows', 'macos', 'linux')


def os_of_asset(name):
    if re.search(r'\.exe$', name, re.I):
        return 'windows'
    if re.search(r'\.dmg$', name, re.I):
        return 'macos'
    if re.search(r'\.(deb|appimage)$', name, re.I):
        return 'linux'
    return None


def count_downloads(releases):
    d = dict.fromkeys(OSES, 0)
    for r in releases:
        if r.get('draft'):
            continue
        for a in r.get('assets') or []:
            os_key = os_of_asset(a.get('name', ''))
            if os_key:
                d[os_key] += int(a.get('download_count') or 0)
    return d


def latest_of(releases):
    for r in releases:
        if r.get('draft') or r.get('prerelease') or not r.get('tag_name'):
            continue
        v = r['tag_name'].lstrip('v')
        return v if re.fullmatch(r'\d+\.\d+\.\d+', v) else None
    return None


def merge(new, old):
    """Новые цифры поверх прошлых: звёзды и версия свежие, загрузки — максимум."""
    old = old or {}
    out = {
        'stars': new['stars'] if new.get('stars') is not None else old.get('stars'),
        'latest': new.get('latest') or old.get('latest'),
        'downloads': None,
    }
    sources = [x for x in (new.get('downloads'), old.get('downloads')) if x]
    if sources:
        out['downloads'] = {k: max(int(s.get(k) or 0) for s in sources) for k in OSES}
    return out


def get(path, token):
    req = urllib.request.Request(
        f'https://api.github.com/repos/{REPO}{path}',
        headers={'Accept': 'application/vnd.github+json', 'User-Agent': 'clipwire-site-stats'},
    )
    if token:
        req.add_header('Authorization', f'Bearer {token}')
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)


def fetch(token):
    new = {'stars': None, 'downloads': None, 'latest': None}
    errors = []
    try:
        new['stars'] = int(get('', token).get('stargazers_count') or 0)
    except Exception as e:  # noqa: BLE001 — любая ошибка сети или ответа
        errors.append(f'repo: {e}')
    try:
        releases = get('/releases?per_page=100', token)
        new['downloads'] = count_downloads(releases)
        new['latest'] = latest_of(releases)
    except Exception as e:  # noqa: BLE001
        errors.append(f'releases: {e}')
    return new, errors


def write_atomic(path, data):
    folder = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(prefix='.stats-', dir=folder)
    with os.fdopen(fd, 'w', encoding='utf-8') as f:
        json.dump(data, f, ensure_ascii=False, separators=(',', ':'))
    os.chmod(tmp, 0o644)  # nginx читает файл от своего пользователя
    os.replace(tmp, path)


def self_test():
    assert os_of_asset('Clipwire_0.9.7_windows_x64-setup.exe') == 'windows'
    assert os_of_asset('Clipwire_0.1.0_x64-setup.exe') == 'windows'
    assert os_of_asset('Clipwire_0.9.7_linux_amd64.AppImage') == 'linux'
    for junk in ('latest.json', 'Clipwire_0.9.7_windows_x64-setup.exe.sig', 'Clipwire_0.9.7_macos_universal.app.tar.gz'):
        assert os_of_asset(junk) is None, junk
    rel = [
        {'tag_name': 'v1.0.0-rc1', 'prerelease': True, 'assets': [{'name': 'a.exe', 'download_count': 5}]},
        {'tag_name': 'v0.9.7', 'assets': [
            {'name': 'Clipwire_0.9.7_windows_x64-setup.exe', 'download_count': 10},
            {'name': 'Clipwire_0.9.7_windows_x64-setup.exe.sig', 'download_count': 99},
            {'name': 'Clipwire_0.9.7_macos_universal.dmg', 'download_count': 3},
            {'name': 'Clipwire_0.9.7_linux_amd64.deb', 'download_count': 2},
            {'name': 'latest.json', 'download_count': 500},
        ]},
        {'tag_name': 'v0.9.6', 'draft': True, 'assets': [{'name': 'b.dmg', 'download_count': 1000}]},
    ]
    assert count_downloads(rel) == {'windows': 15, 'macos': 3, 'linux': 2}
    assert latest_of(rel) == '0.9.7'
    old = {'stars': 40, 'latest': '0.9.7', 'downloads': {'windows': 2000, 'macos': 900, 'linux': 700}}
    m = merge({'stars': None, 'latest': '1.0.0', 'downloads': {'windows': 2100, 'macos': 10, 'linux': 700}}, old)
    assert m == {'stars': 40, 'latest': '1.0.0', 'downloads': {'windows': 2100, 'macos': 900, 'linux': 700}}, m
    assert merge({'stars': 3, 'latest': None, 'downloads': None}, None) == {'stars': 3, 'latest': None, 'downloads': None}
    print('refresh-stats: self-test ok')


def main(argv):
    if argv[1:] == ['--self-test']:
        self_test()
        return 0
    if len(argv) != 2:
        print(__doc__)
        return 2
    path = argv[1]
    try:
        with open(path, encoding='utf-8') as f:
            old = json.load(f)
    except (OSError, ValueError):
        old = None
    new, errors = fetch(os.environ.get('GITHUB_TOKEN'))
    for e in errors:
        print('GitHub:', e, file=sys.stderr)
    if new['stars'] is None and new['downloads'] is None:
        print('GitHub не ответил, прошлый stats.json оставлен как есть', file=sys.stderr)
        return 1
    data = merge(new, old)
    data['updated'] = int(time.time())
    write_atomic(path, data)
    print('stats.json:', json.dumps(data, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
