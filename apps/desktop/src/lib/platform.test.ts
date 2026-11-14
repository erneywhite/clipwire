// node src/lib/platform.test.ts
import assert from 'node:assert/strict'
import { deviceName, keys } from './platform.ts'

assert.equal(keys('Ctrl + P', true), '⌘P')
assert.equal(keys('Ctrl+P', true), '⌘P')
assert.equal(keys('Ctrl P закрепить', true), '⌘P закрепить')
assert.equal(keys('Закрепить (Ctrl+P)', true), 'Закрепить (⌘P)')
assert.equal(keys('изменено · Ctrl+Enter вставить · Ctrl+S сохранить', true), 'изменено · ⌘Enter вставить · ⌘S сохранить')
assert.equal(keys('Ctrl ↵ вставить', true), '⌘↵ вставить')
assert.equal(keys('{kind} (Alt + ← →)', true), '{kind} (⌥← →)')
assert.equal(keys('Shift + Delete', true), '⇧Delete')
assert.equal(keys('Ctrl + 1…9', true), '⌘1…9')
// Не на маке и без клавиш — без изменений.
assert.equal(keys('Ctrl + P', false), 'Ctrl + P')
assert.equal(keys('Controller, Altitude', true), 'Controller, Altitude')
// Значок Apple в имени мака: на маке остаётся, на Windows убирается.
assert.equal(deviceName(' MacBook Pro (Erney)', false), 'MacBook Pro (Erney)')
assert.equal(deviceName('MacBook Pro', false), 'MacBook Pro')
assert.equal(deviceName(' MacBook Pro', true), ' MacBook Pro')
assert.equal(deviceName('ERNEY-PC', false), 'ERNEY-PC')
assert.equal(deviceName('', false), '')
console.log('platform: ok')
