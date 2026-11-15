// Проверка ожидания последней начатой работы. Запуск: node src/lib/inflight.test.ts
// (Node 23.6+ сам убирает типы), как transfer.test.ts.
// shortcut: тесты без фреймворка и вне npm run, переделать при появлении vitest.
const { inFlight } = (await import('./inflight' + '.ts')) as typeof import('./inflight')

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

// Пустой — ждать нечего.
{
  const f = inFlight()
  check(!f.busy, 'без работы не занят')
  await f.settled()
}

// Ждёт, пока работа не кончится.
{
  const f = inFlight()
  let done = false
  f.track(wait(20).then(() => (done = true)))
  check(f.busy, 'работа идёт')
  await f.settled()
  check(done && !f.busy, 'дождались конца')
}

// Работа, начатая, пока ждали, тоже дожидается; старая не снимает новую.
{
  const f = inFlight()
  const order: string[] = []
  f.track(wait(10).then(() => order.push('first')))
  setTimeout(() => f.track(wait(30).then(() => order.push('second'))), 5)
  await f.settled()
  check(order.join() === 'first,second', 'дождались и той, что началась позже')
  check(!f.busy, 'после обеих свободен')
}

// Ранняя кончилась позже поздней: занятость снимает только последняя.
{
  const f = inFlight()
  f.track(wait(30))
  const late = f.track(wait(5))
  await late
  check(!f.busy, 'последняя кончилась — свободен, хоть ранняя ещё идёт')
}

// Ошибка работы не пробрасывается и не держит занятость.
{
  const f = inFlight()
  const p = f.track(Promise.reject(new Error('нет')))
  p.catch(() => {})
  await f.settled()
  check(!f.busy, 'после ошибки свободен')
}

console.log('inflight: всё прошло')
