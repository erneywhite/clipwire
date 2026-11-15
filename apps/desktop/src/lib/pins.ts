// Закрепы в окне вставки: подписи и выбор, как убрать закреп.
import type { PinnedDelete } from './api'
import { t } from './i18n.ts'

/** Длина подписи, как в Rust (`history_note`). */
export const NOTE_MAX = 80

/** Подпись бывает только у закреплённых записей. */
export function canNote(e: { pinned: boolean }): boolean {
  return e.pinned
}

/**
 * Подпись так, как её сохранит Rust (`history::clean_note`): без пробелов по
 * краям, управляющие символы (переводы строк) — пробелами, не длиннее
 * NOTE_MAX символов. Пустая строка — убрать подпись.
 */
export function cleanNote(s: string): string {
  // По символам, а не по UTF-16: эмодзи не режется пополам.
  const chars = [...s.trim()].map((c) => (/\p{Cc}/u.test(c) ? ' ' : c))
  return chars.slice(0, NOTE_MAX).join('').trimEnd()
}

/** Сохранять ли: подпись правда поменялась. */
export function noteChanged(before: string | null, input: string): boolean {
  return cleanNote(input) !== (before ?? '')
}

/** Del на записи: у закреплённой сначала выбор, обычная удаляется сразу. */
export function removeAction(e: { pinned: boolean }): 'choose' | 'remove' {
  return e.pinned ? 'choose' : 'remove'
}

export interface DeleteChoice {
  mode: PinnedDelete
  label: string
  /** Пояснение под пунктом. */
  hint: string
  danger: boolean
}

/** Пункты выбора для закреплённой записи, сверху самый мягкий. */
export function deleteChoices(): DeleteChoice[] {
  return [
    { mode: 'here', label: t('Снять закреп только здесь'), hint: t('на других устройствах останется'), danger: false },
    { mode: 'everywhere', label: t('Снять закреп везде'), hint: t('запись останется в истории'), danger: false },
    { mode: 'delete', label: t('Удалить везде'), hint: t('со всех устройств группы'), danger: true },
  ]
}
