// Подписи и тексты подтверждений для раздела «Устройства группы».
import type { GroupDevice, StorageKind } from './api'
import type * as format from './format'

/** Функции из `format.ts`. Передаются параметром, чтобы модуль запускался в
 *  Node для тестов без сборки (импорт без расширения Node не находит). */
export type Fmt = Pick<typeof format, 'ago' | 'plural'>

/** Ответ на смену ключа потерялся: результат придёт событием. */
export const CHECKING = 'Ответ сервера не пришёл, проверяю…'

/** Сообщение после смены ключа на другом устройстве. */
export function rekeyedBy(name: string): string {
  const n = name.trim()
  return n ? `Ключ группы сменило устройство «${n}»` : 'Ключ группы сменён на другом устройстве'
}

/** Имя для показа: из карточки или «Неизвестное устройство». */
export function deviceTitle(d: GroupDevice): string {
  return d.name?.trim() || 'Неизвестное устройство'
}

/** «в сети», «был в сети: 5 мин», «не в сети». */
export function seenLabel(d: GroupDevice, { ago }: Fmt, now = Date.now()): string {
  if (d.online) return 'в сети'
  if (!d.last_seen_ms) return 'не в сети'
  const a = ago(d.last_seen_ms, now)
  return a === 'сейчас' ? 'не в сети' : `был в сети: ${a}`
}

/** Устройства, которые не получат новый ключ: без карточки и не удаляемое. */
export function leftOut(list: GroupDevice[], removeId: string | null): GroupDevice[] {
  return list.filter((d) => !d.me && !d.known && d.id !== removeId)
}

/**
 * Предупреждение про доступы к внешнему хранилищу у устройств, которые
 * окажутся вне группы: исключаемого (`removing`) и/или оставшихся без
 * нового ключа (`leftOut`, их `lost` штук).
 */
export function exposedAccessText(kind: StorageKind | null, removing = true, lost = 0): string {
  const fix =
    kind === 's3'
      ? 'Смените ключ S3 у поставщика хранилища и сохраните новый в настройках «Хранилище файлов».'
      : kind === 'sftp'
        ? 'Смените пароль или ключ SSH на сервере и сохраните новые в настройках «Хранилище файлов».'
        : ''
  if (!fix || (!removing && lost === 0)) return ''
  const unknown = lost === 1 ? 'у устройства без карточки' : 'у устройств без карточки'
  const who = removing && lost > 0 ? `у исключённого устройства и ${unknown}` : removing ? 'у исключённого устройства' : unknown
  return `Внимание: ${who} остались доступы к хранилищу файлов. ${fix}`
}

/**
 * Объяснение перед подтверждением: что будет с удаляемым и с остальными.
 * `storage` — вид хранилища файлов группы (`null` — не знаем).
 */
export function confirmText(list: GroupDevice[], removeId: string | null, { plural }: Fmt, storage: StorageKind | null = null): string[] {
  const out: string[] = []
  const target = list.find((d) => d.id === removeId)
  if (target) {
    out.push(
      `«${deviceTitle(target)}» потеряет доступ к группе: не получит новые элементы и файлы и не сможет вернуться по старому коду приглашения. То, что уже есть на нём, останется.`,
    )
  }
  const lost = leftOut(list, removeId)
  // Доступы к внешнему хранилищу остаются у всех, кто окажется вне группы:
  // и у исключаемого, и у тех, кому новый ключ не передать.
  const exposed = exposedAccessText(storage, !!target, lost.length)
  if (exposed) out.push(exposed)
  const others = list.filter((d) => !d.me && d.known && d.id !== removeId)
  if (others.length > 0) {
    out.push(
      `${others.length} ${plural(others.length, ['устройство получит', 'устройства получат', 'устройств получат'])} новый ключ сами, когда будут в сети.`,
    )
  }
  if (lost.length > 0) {
    const ids = lost.map((d) => d.fingerprint).join(', ')
    out.push(
      `Внимание: ${lost.length} ${plural(lost.length, ['устройство', 'устройства', 'устройств'])} без карточки (${ids}) новый ключ не получат. Их придётся подключить заново по новому коду.`,
    )
  }
  out.push(
    'История группы на сервере удалится, локальная история на устройствах останется. Код приглашения сменится, старый перестанет работать.',
  )
  return out
}
