// Проверка расчёта виртуальной прокрутки. Запуск: node src/lib/vlist.test.ts
// (Node 23.6+ сам убирает типы), как transfer.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
const { anchored, anchoredById, disjoint, keptInDom, DEFAULT_HEIGHT, HeightCache, offsets, reloadCount, rowAt, rowsLeft, scrollFor, visibleRange } = (await import(
  './vlist' + '.ts'
)) as typeof import('./vlist')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

// Пустой список.
{
  const o = offsets(0, () => 10)
  check(o.length === 1 && o[0] === 0, 'пустой список — одна точка 0')
  const r = visibleRange(o, 0, 500, 10)
  check(r.start === 0 && r.end === 0, 'в пустом списке рисовать нечего')
  check(rowAt(o, 100) === 0, 'строка в пустом списке — 0')
  check(scrollFor(o, 0, 40, 300) === 40, 'в пустом списке прокрутка не меняется')
}

// 1000 строк по 50 px.
{
  const o = offsets(1000, () => 50)
  check(o[1000] === 50_000, 'высота списка — сумма строк')
  check(rowAt(o, 0) === 0 && rowAt(o, 49.9) === 0 && rowAt(o, 50) === 1, 'граница строки относится к следующей')
  check(rowAt(o, 1e9) === 999, 'ниже списка — последняя строка')
  check(rowAt(o, -5) === 0, 'выше списка — первая строка')

  const top = visibleRange(o, 0, 400, 10)
  check(top.start === 0 && top.end === 18, `вверху: 8 видимых и 10 запаса снизу, вышло ${top.start}..${top.end}`)
  const mid = visibleRange(o, 5000, 400, 10)
  check(mid.start === 90 && mid.end === 118, `в середине: запас с двух сторон, вышло ${mid.start}..${mid.end}`)
  const end = visibleRange(o, 49_600, 400, 10)
  check(end.start === 982 && end.end === 1000, `внизу: конец не дальше списка, вышло ${end.start}..${end.end}`)
  const zero = visibleRange(o, 0, 0, 10)
  check(zero.start === 0 && zero.end === 11, 'окно ещё не измерено: одна строка и запас')

  // Прокрутка к выбранной: «nearest».
  check(scrollFor(o, 3, 0, 400) === 0, 'видимая строка — прокрутка на месте')
  check(scrollFor(o, 8, 0, 400) === 50, 'строка ниже окна встаёт к низу')
  check(scrollFor(o, 2, 300, 400) === 100, 'строка выше окна встаёт к верху')
  check(scrollFor(o, 999, 0, 400) === 49_600, 'последняя строка — к самому низу')
  check(scrollFor(o, 1000, 70, 400) === 70, 'индекс за концом — прокрутка на месте')
}

// Разная высота: строка выше окна, чем окно, встаёт верхом.
{
  const hs = [40, 40, 600, 40]
  const o = offsets(hs.length, (i) => hs[i])
  check(o[4] === 720, 'сумма разных высот')
  check(scrollFor(o, 2, 0, 300) === 80, 'высокая строка ниже окна — к верху, а не к низу')
  const r = visibleRange(o, 100, 300, 0)
  check(r.start === 2 && r.end === 3, `внутри высокой строки видна только она, вышло ${r.start}..${r.end}`)
}

// Кэш высот: замеры, средняя для незамеренных, чистка.
{
  const c = new HeightCache()
  check(c.get('a', 'row') === DEFAULT_HEIGHT.row, 'до замеров — оценка по умолчанию')
  check(c.get('x', 'thumb') === DEFAULT_HEIGHT.thumb, 'миниатюра — своя оценка')
  check(c.set('a', 'row', 40) === 40 - DEFAULT_HEIGHT.row, 'set отдаёт разницу с прежней оценкой')
  c.set('b', 'row', 60)
  check(c.get('a', 'row') === 40, 'замеренная строка — свой замер')
  check(c.get('z', 'row') === 50, 'незамеренная — средняя по замеренным того же вида')
  check(c.get('z', 'thumb') === DEFAULT_HEIGHT.thumb, 'средняя по другому виду не смешивается')
  check(c.set('a', 'row', 40) === 0, 'тот же замер — разницы нет')
  c.set('a', 'row', 80)
  check(c.get('z', 'row') === 70, 'повторный замер заменяет старый в средней')
  c.set('t', 'thumb', 100)
  c.keepOnly(new Set(['a', 't']))
  check(c.size === 2 && !c.has('b'), 'keepOnly забывает ушедшие строки')
  check(c.get('z', 'row') === 80, 'средняя пересчитана после чистки')
  c.clear()
  check(c.size === 0 && c.get('a', 'row') === DEFAULT_HEIGHT.row, 'clear сбрасывает всё')
}

// Сколько перечитывать при изменении истории.
{
  check(reloadCount(200, 10, 200, 50) === 200, 'вверху — одна страница')
  check(reloadCount(50, 10, 200, 50) === 200, 'подгружено мало — всё равно страница: могли прийти новые')
  check(reloadCount(5000, 10, 200, 50) === 200, 'вверху после глубокой прокрутки — не всё подгруженное')
  check(reloadCount(5000, 160, 200, 50) === 400, 'запас до конца не меньше ahead')
  check(reloadCount(5000, 1210, 200, 50) === 1400, 'в середине — до видимого конца с запасом')
  check(reloadCount(1300, 1290, 200, 50) === 1300, 'не больше подгруженного')
  for (const needed of [0, 1, 149, 150, 151, 399, 777]) {
    const n = reloadCount(1e6, needed, 200, 50)
    check(n - needed >= 50 && n % 200 === 0, `после перечитывания запас не меньше ahead (needed=${needed}, n=${n})`)
  }
}

// Привязка прокрутки: верхняя видимая строка не уезжает.
{
  const before = offsets(10, () => 50)
  const hs = [50, 120, 50, 50, 50, 50, 50, 50, 50, 50]
  const after = offsets(10, (i) => hs[i])
  check(anchored(before, after, 0) === 0, 'вверху прокрутка не меняется')
  check(anchored(before, after, 60) === 60, 'строка 1 выросла, но она и есть верхняя: место внутри неё то же')
  check(anchored(before, after, 210) === 280, 'строка выше окна выросла на 70 — прокрутка тоже')
  const shrunk = offsets(10, (i) => (i < 3 ? 10 : 50))
  check(anchored(before, shrunk, 175) === 55, 'строки выше сжались — прокрутка меньше')
  check(anchored(before, offsets(11, () => 50), 210) === 210, 'другое число строк — не трогаем')
}

// Замена списка: верхняя видимая запись остаётся на месте.
{
  const ids = ['a', 'b', 'c', 'd', 'e']
  const before = offsets(5, () => 50)
  const withNew = ['n', ...ids]
  const after = offsets(6, (i) => (i === 0 ? 80 : 50))
  check(anchoredById(before, ids, after, withNew, 0) === 0, 'в самом верху новая запись видна, прокрутка не меняется')
  check(anchoredById(before, ids, after, withNew, 120) === 200, 'сверху пришла запись в 80 px — прокрутка ниже на 80')
  const gone = ['a', 'c', 'd', 'e']
  check(anchoredById(before, ids, offsets(4, () => 50), gone, 160) === 110, 'выше удалили запись — прокрутка выше')
  check(anchoredById(before, ids, offsets(2, () => 50), ['x', 'y'], 160) === 160, 'верхней записи больше нет — не трогаем')
}

// Прыжок к строке: положение выставлено по DOM, сдвигается только поле сверху.
{
  // Строки 0..99 по оценке 56 px, отрисованы 80..99 (первая в DOM — 80).
  const before = offsets(100, () => 56)
  // Замеры строк 80..99 по 40 px сдвинули и среднее для незамеренных: 40.
  const after = offsets(100, () => 40)
  const top = 80 * 56 + 300
  const y = keptInDom(before, after, 80, top)
  check(y === top - 80 * 16, `сдвиг ровно на изменение поля сверху: ${y}`)
  // anchored поправил бы ещё и на ошибку оценки отрисованных строк выше верха окна.
  check(anchored(before, after, top) !== y, 'старый расчёт двигает больше, чем поле сверху')
  check(keptInDom(before, offsets(100, (i) => (i < 80 ? 56 : 40)), 80, top) === top, 'поле сверху то же — прокрутка не меняется')
  check(keptInDom(before, offsets(5, () => 40), 80, top) === top, 'другое число строк — не трогаем')
  check(keptInDom(before, after, 0, 10) === 10, 'с первой строки поля нет')
}

// Ширина строк: кэш чистится, только когда она правда изменилась.
{
  const c = new HeightCache()
  check(!c.setWidth(0), 'нулевая ширина — не замер')
  check(!c.setWidth(400), 'первая ширина — чистить нечего')
  c.set('a', 'row', 30)
  check(!c.setWidth(400.2), 'та же ширина с дробью — кэш на месте')
  check(c.has('a'), 'замер остался')
  check(!c.setWidth(0), 'окно спрятали — замеры не трогаем')
  check(c.has('a'), 'после скрытия замер на месте')
  check(c.setWidth(500), 'другая ширина — кэш очищен')
  check(!c.has('a') && c.get('a', 'row') === DEFAULT_HEIGHT.row, 'после очистки снова оценка')
}

check(rowsLeft(100, 10, 20) === 79, 'осталось — от нижней видимой')
check(rowsLeft(100, 95, 20) === 4, 'осталось — от выбранной, если она ниже')

// Прыжок или шаг: остались ли в DOM прежние строки.
check(!disjoint({ start: 0, end: 20 }, { start: 1, end: 21 }), 'шаг вниз на строку — отрезки пересекаются')
check(!disjoint({ start: 10, end: 30 }, { start: 9, end: 29 }), 'шаг вверх на строку — отрезки пересекаются')
check(disjoint({ start: 0, end: 12 }, { start: 139, end: 150 }), 'по кругу с первой на последнюю — прежних строк нет')
check(disjoint({ start: 139, end: 150 }, { start: 0, end: 12 }), 'по кругу с последней на первую — прежних строк нет')
check(disjoint({ start: 0, end: 10 }, { start: 10, end: 20 }), 'отрезки встык — общих строк нет')
check(disjoint({ start: 0, end: 0 }, { start: 0, end: 10 }), 'из пустого списка — прыжок')

console.log('vlist: всё прошло')
