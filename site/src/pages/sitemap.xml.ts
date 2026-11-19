import type { APIRoute } from 'astro';

// Две страницы, каждая со ссылками на языковую пару (hreflang).
export const GET: APIRoute = ({ site }) => {
  if (!site) return new Response('', { status: 404 });
  const u = (p: string) => new URL(p, site).href;
  const today = new Date().toISOString().slice(0, 10);
  const alt = `
    <xhtml:link rel="alternate" hreflang="en" href="${u('/')}"/>
    <xhtml:link rel="alternate" hreflang="ru" href="${u('/ru/')}"/>
    <xhtml:link rel="alternate" hreflang="x-default" href="${u('/')}"/>`;
  const page = (p: string) => `
  <url>
    <loc>${u(p)}</loc>
    <lastmod>${today}</lastmod>${alt}
  </url>`;
  const xml = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">${page('/')}${page('/ru/')}
</urlset>
`;
  return new Response(xml, { headers: { 'Content-Type': 'application/xml; charset=utf-8' } });
};
