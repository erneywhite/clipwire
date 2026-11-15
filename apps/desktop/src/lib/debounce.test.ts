// Проверка отложенного вызова. Запуск: node src/lib/debounce.test.ts
// (Node 23.6+ сам убирает типы), как transfer.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
const { debounce } = (await import('./debounce' + '.ts')) as typeof import('./debounce')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms))

// Серия вызовов — один запуск после паузы.
{
  let n = 0
  const d = debounce(() => n++, 30)
  d()
  d()
  await wait(10)
  d()
  check(d.pending && n === 0, 'пока набирают — не запускается')
  await wait(15)
  check(n === 0, 'отсчёт пошёл заново с последнего вызова')
  await wait(40)
  check(n === 1 && !d.pending, 'после паузы — ровно один запуск')
}

// cancel забывает вызов.
{
  let n = 0
  const d = debounce(() => n++, 20)
  d()
  d.cancel()
  check(!d.pending, 'после cancel ничего не ждёт')
  d.cancel()
  check(!d.pending, 'повторный cancel ничего не ломает')
  await wait(40)
  check(n === 0, 'отменённый вызов не запускается')
}

console.log('debounce: всё прошло')
