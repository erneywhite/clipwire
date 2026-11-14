// Подписи и тексты подтверждений для раздела «Устройства группы».
import type { GroupDevice, StorageKind } from './api'
import type * as format from './format'
import { t, tn, lang } from './i18n.ts'

/** Функции из `format.ts`. Передаются параметром, чтобы модуль запускался в
 *  Node для тестов без сборки (импорт без расширения Node не находит). */
export type Fmt = Pick<typeof format, 'ago' | 'plural'>

/** Ответ на смену ключа потерялся: результат придёт событием. */
export const CHECKING = 'Ответ сервера не пришёл, проверяю…'

/** То же на языке интерфейса. `CHECKING` вычисляется при загрузке модуля, до выбора языка. */
export function checking(): string {
  return t('Ответ сервера не пришёл, проверяю…')
}

/** Сообщение после смены ключа на другом устройстве. */
export function rekeyedBy(name: string): string {
  const n = name.trim()
  return n ? t('Ключ группы сменило устройство «{n}»', { n }) : t('Ключ группы сменён на другом устройстве')
}

/** Имя для показа: из карточки или «Неизвестное устройство». */
export function deviceTitle(d: GroupDevice): string {
  return d.name?.trim() || t('Неизвестное устройство')
}

/** «в сети», «был в сети: 5 мин», «не в сети». */
export function seenLabel(d: GroupDevice, { ago }: Fmt, now = Date.now()): string {
  if (d.online) return t('в сети')
  if (!d.last_seen_ms) return t('не в сети')
  const a = ago(d.last_seen_ms, now)
  if (a === t('сейчас')) return t('не в сети')
  // По-английски «last seen 5 min ago», но «last seen yesterday».
  return lang() === 'en' && /^\d/.test(a) ? t('был в сети: {a} назад', { a }) : t('был в сети: {a}', { a })
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
      ? t('Смените ключ S3 у поставщика хранилища и сохраните новый в настройках «Хранилище файлов».')
      : kind === 'sftp'
        ? t('Смените пароль или ключ SSH на сервере и сохраните новые в настройках «Хранилище файлов».')
        : ''
  if (!fix || (!removing && lost === 0)) return ''
  const unknown = lost === 1 ? t('у устройства без карточки') : t('у устройств без карточки')
  const who = removing && lost > 0 ? t('у исключённого устройства и {unknown}', { unknown }) : removing ? t('у исключённого устройства') : unknown
  return t('Внимание: {who} остались доступы к хранилищу файлов. {fix}', { who, fix })
}

/**
 * Объяснение перед подтверждением: что будет с удаляемым и с остальными.
 * `storage` — вид хранилища файлов группы (`null` — не знаем).
 */
export function confirmText(list: GroupDevice[], removeId: string | null, _fmt: Fmt, storage: StorageKind | null = null): string[] {
  const out: string[] = []
  const target = list.find((d) => d.id === removeId)
  if (target) {
    out.push(
      t('«{name}» потеряет доступ к группе: не получит новые элементы и файлы и не сможет вернуться по старому коду приглашения. То, что уже есть на нём, останется.', {
        name: deviceTitle(target),
      }),
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
      tn(
        others.length,
        ['устройство получит новый ключ сами, когда будет в сети.', 'устройства получат новый ключ сами, когда будут в сети.', 'устройств получат новый ключ сами, когда будут в сети.'],
        ['{n} device will get the new key on its own when it comes online.', '{n} devices will get the new key on their own when they come online.'],
      ),
    )
  }
  if (lost.length > 0) {
    const ids = lost.map((d) => d.fingerprint).join(', ')
    out.push(
      t('Внимание: {count} без карточки ({ids}) новый ключ не получат. Их придётся подключить заново по новому коду.', {
        count: tn(lost.length, ['устройство', 'устройства', 'устройств'], ['device', 'devices']),
        ids,
      }),
    )
  }
  out.push(t('История группы на сервере удалится, локальная история на устройствах останется. Код приглашения сменится, старый перестанет работать.'))
  return out
}
