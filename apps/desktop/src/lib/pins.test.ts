// Проверка подписей и выбора при удалении закрепа. Запуск: node src/lib/pins.test.ts
// (Node 23.6+ сам убирает типы), как остальные *.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
const { canNote, cleanNote, deleteChoices, NOTE_MAX, noteChanged, removeAction } = (await import('./pins' + '.ts')) as typeof import('./pins')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

check(canNote({ pinned: true }) && !canNote({ pinned: false }), 'подпись только у закреплённых')

check(cleanNote('  пароль от роутера  ') === 'пароль от роутера', 'пробелы по краям')
// Как в Rust: каждый управляющий символ — пробел, без схлопывания.
check(cleanNote('две\r\nстроки\n') === 'две  строки', cleanNote('две\r\nстроки\n'))
check(cleanNote('   ') === '', 'пустая — убрать подпись')
check([...cleanNote('я'.repeat(100))].length === NOTE_MAX, 'не длиннее 80')
// 80 эмодзи — 160 единиц UTF-16: режется по символам, а не пополам.
const emoji = cleanNote('😀'.repeat(90))
check([...emoji].length === NOTE_MAX && !/\uD83D$/.test(emoji), 'эмодзи не режется')
check(cleanNote('a'.repeat(79) + ' b') === 'a'.repeat(79), 'пробел на границе обрезки')

check(!noteChanged(null, ''), 'не было и нет')
check(!noteChanged('vpn', ' vpn '), 'те же слова')
check(noteChanged('vpn', ''), 'убрали')
check(noteChanged(null, 'vpn'), 'добавили')

check(removeAction({ pinned: true }) === 'choose', 'закреп — выбор')
check(removeAction({ pinned: false }) === 'remove', 'обычная — сразу')

const modes = deleteChoices().map((c) => c.mode)
check(modes.join(',') === 'here,everywhere,delete', modes.join(','))
check(deleteChoices().filter((c) => c.danger).map((c) => c.mode).join() === 'delete', 'опасный только «удалить везде»')

console.log('pins: всё прошло')
