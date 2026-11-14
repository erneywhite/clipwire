/// <reference types="node" />
// Проверка перевода. Запуск: node src/lib/i18n.test.ts (Node 23.6+ сам
// убирает типы), как остальные *.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
//
// 1. Каждая строка из t('…') в src есть в i18n-en.ts, с теми же {подстановками}.
// 2. В t() только строковый литерал без ${…}: иначе строку не найти и не перевести.
// 3. Формы tn()/form() передаются прямо в вызове, словарь им не нужен.
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const en = ((await import('./i18n-en' + '.ts')) as { default: Record<string, string> }).default
const i18n = (await import('./i18n' + '.ts')) as typeof import('./i18n')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

// Сами функции.
check(i18n.t('Привет, {name}', { name: 'мир' }) === 'Привет, мир', 'подстановка по-русски')
check(i18n.tn(5, ['файл', 'файла', 'файлов'], ['file', 'files']) === '5 файлов', 'русское множественное')
check(i18n.tn(22, ['файл', 'файла', 'файлов'], ['file', 'files']) === '22 файла', '22 файла')
check(i18n.resolveLang('system', 'ru-RU') === 'ru' && i18n.resolveLang('system', 'lv-LV') === 'en', 'язык системы')
check(i18n.resolveLang('ru', 'en-US') === 'ru', 'явный выбор важнее системы')
i18n.setLang('en')
check(i18n.tn(1, ['файл', 'файла', 'файлов'], ['file', 'files']) === '1 file', 'английское единственное')
check(i18n.tn(3, ['файл', 'файла', 'файлов'], ['{n} more file', '{n} more files']) === '3 more files', '{n} в форме')
check(i18n.t('нет такой строки') === 'нет такой строки', 'без перевода — русский текст')
i18n.setLang('ru')

const root = fileURLToPath(new URL('..', import.meta.url))
function files(dir: string): string[] {
  return readdirSync(dir).flatMap((name: string) => {
    const p = join(dir, name)
    if (statSync(p).isDirectory()) return files(p)
    if (!/\.(ts|svelte)$/.test(name) || /\.test\.ts$|i18n(-en)?\.ts$/.test(name) || p.includes('i18n-en')) return []
    return [p]
  })
}

const vars = (s: string) =>
  [...s.matchAll(/\{(\w+)\}/g)]
    .map((m) => m[1])
    .sort()
    .join(',')
const used = new Set<string>()
const problems: string[] = []
// t( с литералом: '…', "…" или `…`. Перед t не буква и не точка — чтобы не ловить split( и obj.t(.
const call = /(?<![\w.$])t\(\s*(['"`])((?:\\.|(?!\1)[^\\])*)\1/g
for (const file of files(root)) {
  const src = readFileSync(file, 'utf8')
  for (const m of src.matchAll(call)) {
    const key = m[2].replace(/\\(['"`\\])/g, '$1').replace(/\\n/g, '\n')
    if (m[1] === '`' && key.includes('${')) {
      problems.push(`${file}: t(\`…\${…}\`) — используйте {подстановку}: ${key}`)
      continue
    }
    used.add(key)
    if (!(key in en)) problems.push(`${file}: нет перевода: ${JSON.stringify(key)}`)
    else if (vars(key) !== vars(en[key])) problems.push(`${file}: подстановки расходятся: ${JSON.stringify(key)}`)
  }
  // t( не с литералом — перевод не найти.
  for (const m of src.matchAll(/(?<![\w.$])t\(\s*(?![\s'"`)])/g)) {
    const line = src.slice(0, m.index).split('\n').length
    problems.push(`${file}:${line}: t() не со строкой — перевод не проверить`)
  }
}
// Один ключ в двух частях словаря — второй молча перекроет первый.
const parts = ['settings', 'popup', 'panels', 'lib']
const seen = new Map<string, string>()
for (const part of parts) {
  const dict = ((await import(`./i18n-en/${part}.ts`)) as { default: Record<string, string> }).default
  for (const k of Object.keys(dict)) {
    if (seen.has(k) && seen.get(k) !== dict[k]) problems.push(`ключ в двух частях с разным переводом (${seen.get(k)} / ${part}): ${JSON.stringify(k)}`)
    seen.set(k, dict[k])
  }
}
const unused = Object.keys(en).filter((k) => !used.has(k))
if (unused.length) console.log(`лишние переводы (строк в коде уже нет): ${unused.length}\n  ${unused.join('\n  ')}`)
if (problems.length) throw new Error(`перевод неполный (${problems.length}):\n${problems.join('\n')}`)
console.log(`i18n: всё прошло, строк: ${used.size}`)
