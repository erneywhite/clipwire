// Последняя начатая работа, которую надо уметь дождаться. Окну вставки:
// Enter и стрелки после смены запроса должны сработать по новому списку, а
// чтение редкого слова идёт полным проходом по истории и может занять
// сотни миллисекунд.

export interface InFlight {
  /** Запомнить работу. Более ранняя, если ещё идёт, больше не ждётся. */
  track<T>(p: Promise<T>): Promise<T>
  /** Работа идёт. */
  readonly busy: boolean
  /** Дождаться последней работы, включая начатые, пока ждали. Ошибки не пробрасывает. */
  settled(): Promise<void>
}

export function inFlight(): InFlight {
  let current: Promise<unknown> | null = null
  return {
    track(p) {
      current = p
      const clear = () => {
        if (current === p) current = null
      }
      p.then(clear, clear)
      return p
    },
    get busy() {
      return current !== null
    },
    async settled() {
      while (current) {
        try {
          await current
        } catch {
          // Ошибку покажет тот, кто начал работу.
        }
      }
    },
  }
}
