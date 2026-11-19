import type { APIRoute } from 'astro';
import { strings } from '../i18n/strings';
import { buildVersion, REPO } from '../lib/release';

// llms.txt — короткая выжимка о проекте для ИИ-ассистентов (llmstxt.org).
// Собирается из тех же текстов, что и страница, чтобы не расходиться с ней.
export const GET: APIRoute = ({ site }) => {
  const t = strings.en;
  const v = buildVersion();
  const gh = `https://github.com/${REPO}`;
  const md = `# Clipwire

> ${t.description}

Clipwire is a free, open-source (AGPL-3.0) shared clipboard for desktop computers. Copy on one computer, paste on another. Latest version: ${v}.

- Platforms: Windows (x64); macOS 13 Ventura or newer, Apple Silicon and Intel; Linux x86-64 on X11 and Wayland (GNOME, KDE Plasma, Sway, Hyprland). No phone apps.
- Syncs plain and formatted text (HTML, RTF), images, files and folders.
- End-to-end encrypted: the group key lives only on the devices, the server sees ciphertext.
- Self-hosted server: one Docker container (ghcr.io/erneywhite/clipwire-server) with a built-in iroh relay. Devices on the same network connect directly (P2P).
- File storage: server disk, S3-compatible storage or SFTP.
- Works without a server as a local, encrypted clipboard history with retention up to "forever".
- Paste window next to the mouse cursor with search, pins, previews and an image editor.
- Exclusions by file extension, name, mask or size; honours .gitignore in copied folders; password managers excluded by default.
- Can import history from Paste (macOS).
- Alternative to Apple Universal Clipboard and Paste that works across Windows, macOS and Linux.

## Links

- [Website](${site ? new URL('/', site).href : gh})
- [Source code and documentation](${gh}): README with setup instructions
- [Latest release downloads](${gh}/releases/latest)
- [Self-hosted server setup](${gh}#self-hosted-server)
- [Support the author on Ko-fi](https://ko-fi.com/erneywhite)

## FAQ

${t.faq.items.map((f) => `### ${f.q}\n\n${f.a}`).join('\n\n')}
`;
  return new Response(md, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
