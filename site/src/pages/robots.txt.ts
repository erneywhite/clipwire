import type { APIRoute } from 'astro';

// Индексировать можно всё. ИИ-поисковики перечислены явно: так их точно не
// отсечёт ни одно общее правило, а сайт попадёт в их ответы.
const AI_BOTS = ['GPTBot', 'OAI-SearchBot', 'ChatGPT-User', 'ClaudeBot', 'Claude-SearchBot', 'PerplexityBot', 'Google-Extended', 'Applebot-Extended', 'CCBot'];

export const GET: APIRoute = ({ site }) => {
  const lines = ['User-agent: *', 'Allow: /', ''];
  for (const bot of AI_BOTS) lines.push(`User-agent: ${bot}`, 'Allow: /', '');
  if (site) lines.push(`Sitemap: ${new URL('/sitemap.xml', site).href}`);
  return new Response(lines.join('\n') + '\n', { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
