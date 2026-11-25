// Подписи и тексты подтверждений для раздела «Устройства группы».
import type { GroupAdmin, GroupDevice, RekeyOffer, StorageKind } from './api'
import type * as format from './format'
import { t, tn, lang } from './i18n.ts'
import { deviceName } from './platform.ts'

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
  return deviceName(d.name ?? '') || t('Неизвестное устройство')
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

/** Кому можно передать новый ключ: устройства с карточкой, кроме этого и удаляемого. */
export function recipients(list: GroupDevice[], removeId: string | null): GroupDevice[] {
  return list.filter((d) => !d.me && d.known && d.id !== removeId)
}

/** Сутки: устройство, которое здесь впервые увидели позже, в списке получателей выделено. */
export const FRESH_MS = 24 * 60 * 60 * 1000

/** Карточку устройства здесь впервые увидели меньше суток назад. */
export function isFresh(d: GroupDevice, now = Date.now()): boolean {
  return !d.me && d.known && d.first_seen_ms > 0 && now - d.first_seen_ms < FRESH_MS
}

/** В списке есть другое устройство с таким же именем. Имя устройство выбирает
 *  само, так что исключаемое могло войти ещё раз под именем соседнего:
 *  различает их только отпечаток. */
export function sameName(list: GroupDevice[], d: GroupDevice): boolean {
  if (!d.known) return false
  const name = deviceTitle(d).toLowerCase()
  return list.some((x) => x.id !== d.id && x.known && deviceTitle(x).toLowerCase() === name)
}

/** Кто останется в группе с новым ключом: это устройство и получатели с галочкой. */
export function staying(list: GroupDevice[], removeId: string | null, unticked: string[]): GroupDevice[] {
  return list.filter((d) => d.me || (d.known && d.id !== removeId && !unticked.includes(d.id)))
}

/** «в группе давно», «появилось: 5 мин назад». Время — по записям этого устройства. */
export function firstSeenLabel(d: GroupDevice, { ago }: Fmt, now = Date.now()): string {
  if (!d.first_seen_ms) return t('в группе давно')
  const a = ago(d.first_seen_ms, now)
  if (a === t('сейчас')) return t('появилось только что')
  return lang() === 'en' && /^\d/.test(a) ? t('появилось: {a} назад', { a }) : t('появилось: {a}', { a })
}

/**
 * Что уходит в команду смены ключа: `confirmed` — получатели, которых человек
 * видел и оставил с галочкой; `remove` — получатели, с которых он галочку снял
 * (они исключаются вместе с удаляемым). `list` — список, показанный в диалоге,
 * а не свежий: если к моменту смены в группе окажется кто-то ещё, движок
 * смену отменит.
 */
export function rekeyChoice(list: GroupDevice[], removeId: string | null, unticked: string[]): { confirmed: string[]; remove: string[] } {
  const all = recipients(list, removeId)
  return {
    confirmed: all.filter((d) => !unticked.includes(d.id)).map((d) => d.id),
    remove: all.filter((d) => unticked.includes(d.id)).map((d) => d.id),
  }
}

/** Предупреждение над списком получателей, если среди отмеченных есть недавние. */
export function freshWarning(list: GroupDevice[], removeId: string | null, unticked: string[], now = Date.now()): string {
  const stay = staying(list, removeId, unticked)
  const fresh = stay.some((d) => isFresh(d, now))
  const twins = stay.some((d) => !d.me && sameName(stay, d))
  if (!fresh && !twins) return ''
  return fresh
    ? t('Внимание: выделенные устройства появились в группе меньше суток назад. Если вы их не подключали, снимите с них галочку: новый ключ они не получат.')
    : t('Внимание: в группе есть устройства с одинаковым именем. Сверьте отпечатки и снимите галочку с того, которое не ваше.')
}

/** Сервер отменил бы смену ключа: в группе появилось устройство, которого не было в диалоге. */
export function isNewDeviceError(msg: string): boolean {
  return msg.includes('В группе появилось новое устройство') || msg.includes('A new device has joined the group')
}

/** Вопрос на устройстве, которое получило новый ключ от другого устройства. */
export function offerText(o: RekeyOffer, when: string): string[] {
  const name = deviceName(o.name) || t('Неизвестное устройство')
  return [
    t('Устройство «{name}» ({os}, отпечаток {fp}) сменило ключ группы. Новый ключ пришёл сюда {when}. Перейти на него?', {
      name,
      os: o.os || '?',
      fp: o.fingerprint,
      when,
    }),
    t('Пока вы не ответили, синхронизация стоит: скопированное в группу не уходит. Если ключ меняли вы, сверьте отпечаток с тем, что показывает то устройство в разделе «Устройства группы», и переходите.'),
    t('Если вы ключ не меняли, не переходите. Новый ключ мог прислать тот, кого удалили из группы, и тогда всё скопированное уйдёт к нему.'),
  ]
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

/** Что остаётся у исключённого устройства на сервере и как это закрыть. */
export function serverTokenText(): string {
  return t(
    'Токен доступа к серверу у исключённого устройства остаётся: с ним оно достучится до relay, но в группу не войдёт и новую группу не создаст. Если устройство могло попасть в чужие руки, смените токен доступа в разделе «Сервер» (нужен код сервера администратора) и раздайте новый код приглашения.',
  )
}

/**
 * Ключ администратора сервера при удалении устройства. Был ли он у
 * удаляемого, не знает никто, кроме самого удаляемого: смена ключа группы
 * его не отзывает. `serverAdmin` — ключ есть у этого устройства: тогда текст
 * поясняет галочку «Сменить ключ администратора сервера», иначе это
 * предупреждение для администратора сервера.
 */
export function adminKeyText(serverAdmin: boolean): string {
  return serverAdmin
    ? t('Если у удалённого устройства был код сервера администратора, он и дальше даёт ему управлять сервером: удалять группы, менять токен доступа и настройки, выдавать коды создания группы. Новый ключ сохранится на этом устройстве, новый код сервера появится здесь, на других устройствах администратора его нужно будет ввести заново.')
    : t('Внимание: если у удалённого устройства был ключ администратора сервера, он у него остался, и с ним можно удалять группы, менять токен доступа и настройки сервера. Попросите администратора сервера сменить ключ администратора и токен доступа.')
}

/**
 * Дождаться, пока синхронизация снова на связи (после смены ключа группы
 * она перезапускается): запросы администратора идут только по живому
 * соединению. `false` — за `timeoutMs` связь не появилась.
 */
export async function waitOnline(
  status: () => Promise<{ kind: string }>,
  timeoutMs = 30_000,
  stepMs = 500,
  sleep: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
): Promise<boolean> {
  for (let waited = 0; ; waited += stepMs) {
    try {
      if ((await status()).kind === 'online') return true
    } catch {
      // Статус не прочитался: попробовать ещё раз.
    }
    if (waited >= timeoutMs) return false
    await sleep(stepMs)
  }
}

/** Админы группы по ответу `group_admin`; `null` — не знаем (нет связи). */
export type Roles = GroupAdmin | null

/** Может ли это устройство удалять устройства и менять ключ группы. Сервер
 *  старше 0.5.0 и группа без админа разрешают это всем; не знаем — не
 *  запрещаем, сервер всё равно проверит сам. */
export function canManage(roles: Roles, list: GroupDevice[]): boolean {
  if (!roles || !roles.supported || roles.admins.length === 0) return true
  return list.some((d) => d.me && roles.admins.includes(d.id))
}

/** Устройство — админ группы. */
export function isAdmin(roles: Roles, d: GroupDevice): boolean {
  return !!roles?.supported && roles.admins.includes(d.id)
}

/** Админы, которые есть в списке устройств группы. Админ старой группы,
 *  ещё не вошедший с новым ключом, в списке не виден и не считается: сервер
 *  тоже не даёт оставить группу только с таким админом. */
function listedAdmins(roles: Roles, list: GroupDevice[]): string[] {
  if (!roles?.supported) return []
  return roles.admins.filter((id) => list.some((d) => d.id === id))
}

/** Как называть админа, которого нет в списке: оставили при смене ключа,
 *  но с новым ключом он ещё не вошёл. */
export function pendingAdminTitle(): string {
  return t('Устройство, которое ещё не вошло с новым ключом')
}

/** Админы, которых нет в списке устройств (см. `listedAdmins`), строками для
 *  списка: их можно снять, если это устройство само админ. Отпечаток — как у
 *  остальных, первые 5 байт id. */
export function pendingAdmins(roles: Roles, list: GroupDevice[]): GroupDevice[] {
  if (!roles?.supported || !roles.multi || list.length === 0) return []
  return roles.admins
    .filter((id) => !list.some((d) => d.id === id))
    .map((id) => ({
      id,
      fingerprint: id.slice(0, 10),
      name: null,
      os: null,
      known: false,
      me: false,
      online: false,
      last_seen_ms: 0,
      first_seen_ms: 0,
    }))
}

/** Устройство — единственный админ группы из списка: снять с него роль нельзя. */
export function lastAdmin(roles: Roles, list: GroupDevice[], d: GroupDevice): boolean {
  const listed = listedAdmins(roles, list)
  return listed.length === 1 && listed[0] === d.id
}

/**
 * Что админ может сделать с ролью устройства в строке списка (сервер с
 * несколькими админами): назначить админом, снять роль (и с себя) или ничего.
 * Админом назначают только устройство с карточкой: без неё не видно, кто это.
 * На старом сервере роль только передаётся, там `null`.
 */
export function adminAction(roles: Roles, list: GroupDevice[], d: GroupDevice): 'grant' | 'revoke' | null {
  if (!roles?.supported || !roles.multi) return null
  if (!list.some((x) => x.me && isAdmin(roles, x))) return null
  if (isAdmin(roles, d)) return 'revoke'
  return d.known ? 'grant' : null
}

/** Пояснение под списком: кто админ и что это значит для этого устройства. */
export function rolesText(roles: Roles, list: GroupDevice[]): string {
  if (!roles || !roles.supported) return ''
  if (roles.admins.length === 0) {
    return t('У группы пока нет админа, поэтому ключ группы может сменить любое устройство. Админом станет первое устройство, которое подключится с кодом сервера администратора.')
  }
  // Админ не из списка ещё не вошёл с новым ключом: имени у него здесь нет.
  const nameOf = (id: string) => {
    const dev = list.find((d) => d.id === id)
    return dev ? `«${deviceTitle(dev)}»` : t('устройство {fp}, которое ещё не вошло с новым ключом', { fp: id.slice(0, 10) })
  }
  const meAdmin = list.some((d) => d.me && roles.admins.includes(d.id))
  if (roles.multi) {
    if (meAdmin && listedAdmins(roles, list).length === 1) {
      return t('Это устройство — единственный админ группы: оно удаляет устройства, меняет ключ группы и назначает других админов. Снять роль с себя можно, когда в группе есть другой админ.')
    }
    if (meAdmin) {
      return t('Это устройство — админ группы. Админы удаляют устройства, меняют ключ группы и назначают или снимают других админов.')
    }
    return t('Удалять устройства и менять ключ группы могут только админы группы: {names}.', {
      names: roles.admins.map(nameOf).join(', '),
    })
  }
  if (meAdmin) {
    return t('Это устройство — админ группы: только оно удаляет устройства и меняет ключ группы. Роль можно передать другому устройству.')
  }
  return t('Удалять устройства и менять ключ группы может только админ группы — {name}. Передать роль другому устройству может только он.', { name: nameOf(roles.admins[0]) })
}

/** Заголовок и пояснение подтверждения: назначить админом, снять роль с
 *  другого устройства или с себя. */
export function adminConfirmText(d: GroupDevice, revoke: boolean, pending = false): { title: string; hint: string } {
  if (pending) {
    return {
      title: t('Снять админа с устройства {fp}?', { fp: d.fingerprint }),
      hint: t('Оно ещё не вошло в группу с новым ключом. Если войдёт, останется в группе, но уже не админом.'),
    }
  }
  const name = deviceTitle(d)
  if (!revoke) {
    return {
      title: t('Сделать «{name}» админом группы?', { name }),
      hint: t('Оно сможет удалять устройства, менять ключ группы и назначать или снимать админов, в том числе это устройство.'),
    }
  }
  if (d.me) {
    return {
      title: t('Перестать быть админом?'),
      hint: t('Это устройство больше не сможет удалять устройства и менять ключ группы. Вернуть роль сможет другой админ.'),
    }
  }
  return {
    title: t('Снять админа с «{name}»?', { name }),
    hint: t('Оно больше не сможет удалять устройства и менять ключ группы.'),
  }
}

/**
 * Объяснение перед подтверждением: что будет с удаляемым и с остальными.
 * `storage` — вид хранилища файлов группы (`null` — не знаем).
 */
export function confirmText(list: GroupDevice[], removeId: string | null, _fmt: Fmt, storage: StorageKind | null = null, unticked: string[] = []): string[] {
  const out: string[] = []
  const target = list.find((d) => d.id === removeId)
  // Получатели, с которых сняли галочку: исключаются вместе с удаляемым.
  const dropped = recipients(list, removeId).filter((d) => unticked.includes(d.id))
  if (target) {
    out.push(
      t('«{name}» потеряет доступ к группе: не получит новые элементы и файлы и не сможет вернуться по старому коду приглашения. То, что уже есть на нём, останется.', {
        name: deviceTitle(target),
      }),
    )
  }
  if (dropped.length > 0) {
    out.push(
      t('Без галочки: {names}. Они тоже потеряют доступ к группе.', {
        names: dropped.map((d) => `«${deviceTitle(d)}» (${d.fingerprint})`).join(', '),
      }),
    )
  }
  const lost = leftOut(list, removeId)
  // Доступы к внешнему хранилищу остаются у всех, кто окажется вне группы:
  // и у исключаемого, и у тех, кому новый ключ не передать.
  const removing = !!target || dropped.length > 0
  const exposed = exposedAccessText(storage, removing, lost.length)
  if (exposed) out.push(exposed)
  // Токен сервера из приглашения у исключённого остаётся: с ним можно создавать
  // группы и занимать квоту файлов. Сменить его можно только на сервере.
  if (removing) out.push(serverTokenText())
  const others = recipients(list, removeId).filter((d) => !unticked.includes(d.id))
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

/**
 * Показывать ли «Стать админом группы» администратору сервера: сервер знает
 * роли, это устройство в списке и ещё не админ. Нужен, когда прежний админ
 * потерян (сломан, продан): без админа ключ группы не сменить и устройства
 * не удалить. Группа без админа получает его и так, при входе с ключом.
 */
export function canClaimAdmin(roles: Roles, list: GroupDevice[], serverAdmin: boolean): boolean {
  if (!serverAdmin || !roles?.supported || roles.admins.length === 0) return false
  const me = list.find((d) => d.me)
  return !!me && !isAdmin(roles, me)
}
