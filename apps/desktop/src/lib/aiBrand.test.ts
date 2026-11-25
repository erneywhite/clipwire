// Запуск: node src/lib/aiBrand.test.ts
import assert from 'node:assert/strict'
import { aiBrand } from './aiBrand.ts'

assert.equal(aiBrand('claude-code'), '#d97757')
assert.equal(aiBrand('Claude Desktop'), '#d97757')
assert.equal(aiBrand('gemini-cli'), '#4285f4')
assert.equal(aiBrand('Visual Studio Code'), '#2f80ed')
assert.equal(aiBrand('GitHub Copilot'), '#2f80ed')
assert.equal(aiBrand('windsurf'), '#0fb5a5')
assert.equal(aiBrand('codex-mcp-client'), null)
assert.equal(aiBrand('cursor-vscode'), null)
assert.equal(aiBrand('clipwire-live-test'), null)
console.log('aiBrand ok')
