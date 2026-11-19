// Живые части страницы: сцена на главном экране, разделы по прокрутке,
// окно вставки, которым можно управлять, и кнопки скачивания под систему.
import { gsap } from 'gsap';
import { ScrollTrigger } from 'gsap/ScrollTrigger';
import { MotionPathPlugin } from 'gsap/MotionPathPlugin';
import { DrawSVGPlugin } from 'gsap/DrawSVGPlugin';
import { ScrambleTextPlugin } from 'gsap/ScrambleTextPlugin';
import { Flip } from 'gsap/Flip';

gsap.registerPlugin(ScrollTrigger, MotionPathPlugin, DrawSVGPlugin, ScrambleTextPlugin, Flip);

type Row = { kind: string; text: string; meta: string; tag?: string };
type Data = {
  rows: Row[];
  glyph: Record<string, string>;
  tryRows: Row[];
  copied: string;
  noResults: string;
  targetEmpty: string;
  version: string;
  repo: string;
  hero: { ctaFor: string; ctaAny: string; ctaMobile: string };
  yours: string;
};

const data: Data = JSON.parse(document.getElementById('scene-data')!.textContent!);
const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector<T>(sel)!;
const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) =>
  Array.from(root.querySelectorAll<T>(sel));

const reduceQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
const CIPHER = 'q7Vx2mN0+LkR9tYwB4eZp1Hs8cUf3GjD6aQiT5oWnKr=';
const HEX = '0123456789abcdef';

const icon = (name: string) =>
  `<svg class="ic" viewBox="0 0 24 24" aria-hidden="true">${data.glyph[name] ?? ''}</svg>`;
const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]!);

/** Строка окна вставки — та же разметка, что в PasteWindow.astro. */
function rowHtml(r: Row, num?: number) {
  const body =
    r.kind === 'image'
      ? `<span class="pw-thumb" aria-label="${esc(r.text)}"><span class="mini-chart" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i><i></i><i></i></span></span>`
      : `<span class="pw-text${r.kind === 'code' ? ' mono' : ''}">${esc(r.text)}</span>`;
  const tag = r.tag ? `<b class="pw-tag">${esc(r.tag)}</b>` : '';
  const n = num !== undefined && num < 9 ? `<span class="pw-num">${num + 1}</span>` : '';
  return `<span class="pw-glyph">${icon(r.kind)}</span><span class="pw-body">${body}<span class="pw-meta">${tag}${esc(r.meta)}</span></span>${n}`;
}

// ── Шапка ──────────────────────────────────────────────────────────────
const top = $('[data-top]');
const onScroll = () => top.classList.toggle('scrolled', window.scrollY > 8);
window.addEventListener('scroll', onScroll, { passive: true });
onScroll();

// ── Система пользователя и ссылки на скачивание ───────────────────────
type OS = 'windows' | 'macos' | 'linux' | 'mobile' | null;
function detectOS(): OS {
  const ua = navigator.userAgent;
  const nav = navigator as Navigator & { userAgentData?: { platform?: string; mobile?: boolean } };
  const p = (nav.userAgentData?.platform || navigator.platform || '').toLowerCase();
  if (nav.userAgentData?.mobile || /android|iphone|ipad|ipod/i.test(ua) || (p === 'macintel' && navigator.maxTouchPoints > 1))
    return 'mobile';
  if (p.includes('win') || /windows/i.test(ua)) return 'windows';
  if (p.includes('mac') || /mac os/i.test(ua)) return 'macos';
  if (p.includes('linux') || /linux|x11|cros/i.test(ua)) return 'linux';
  return null;
}
const OS_NAME: Record<string, string> = { windows: 'Windows', macos: 'macOS', linux: 'Linux' };
const OS_SUFFIX: Record<string, string> = { windows: '_windows_x64-setup.exe', macos: '_macos_universal.dmg' };
const assetUrl = (v: string, suffix: string) =>
  `https://github.com/${data.repo}/releases/download/v${v}/Clipwire_${v}${suffix}`;

const os = detectOS();
const cta = $<HTMLAnchorElement>('[data-cta]');
const ctaLabel = $('[data-cta-label]');

function applyVersion(v: string) {
  $$('[data-version]').forEach((el) => (el.textContent = v));
  $$<HTMLAnchorElement>('a[data-suffix]').forEach((a) => (a.href = assetUrl(v, a.dataset.suffix!)));
  if (os && OS_SUFFIX[os]) cta.href = assetUrl(v, OS_SUFFIX[os]);
}

if (os === 'mobile') {
  ctaLabel.textContent = data.hero.ctaMobile;
} else if (os) {
  ctaLabel.textContent = `${data.hero.ctaFor} ${OS_NAME[os]}`;
  $(`.os[data-os="${os}"]`)?.classList.add('mine');
}
applyVersion(data.version);

// Сайт заливается руками и может отстать от выпусков: спрашиваем GitHub о
// последней версии и подменяем ссылки. Не ответил — остаются ссылки сборки.
(async () => {
  const key = 'clipwire-latest';
  let latest: string | null = null;
  try {
    latest = sessionStorage.getItem(key);
  } catch {
    /* хранилище недоступно — просто спросим */
  }
  if (!latest) {
    try {
      const r = await fetch(`https://api.github.com/repos/${data.repo}/releases/latest`, {
        headers: { Accept: 'application/vnd.github+json' },
      });
      if (r.ok) latest = String((await r.json()).tag_name || '').replace(/^v/, '') || null;
    } catch {
      /* нет сети или лимит API */
    }
    try {
      if (latest) sessionStorage.setItem(key, latest);
    } catch {
      /* не важно */
    }
  }
  if (latest && /^\d+\.\d+\.\d+$/.test(latest) && latest !== data.version) applyVersion(latest);
})();

// ── Главный экран: провод ──────────────────────────────────────────────
const scene = $('[data-scene]');
const devA = $('[data-dev-a]');
const devB = $('[data-dev-b]');
const wire = $<SVGSVGElement>('[data-wire]');
const wireBase = $<SVGPathElement>('[data-wire-base]');
const wireGlow = $<SVGPathElement>('[data-wire-glow]');
const endA = $<SVGCircleElement>('[data-wire-a]');
const endB = $<SVGCircleElement>('[data-wire-b]');
const packet = $('[data-packet]');
const packetText = $('[data-packet-text]');
const heroList = $('#hero-pw [data-pw-list]');

/** Ломаная с закруглёнными углами, как провод на знаке. */
function roundedPath(pts: [number, number][], r = 16) {
  let d = `M${pts[0][0]} ${pts[0][1]}`;
  for (let i = 1; i < pts.length - 1; i++) {
    const [px, py] = pts[i - 1];
    const [x, y] = pts[i];
    const [nx, ny] = pts[i + 1];
    const l1 = Math.hypot(x - px, y - py);
    const l2 = Math.hypot(nx - x, ny - y);
    const rr = Math.min(r, l1 / 2, l2 / 2);
    const ax = x - ((x - px) / l1) * rr;
    const ay = y - ((y - py) / l1) * rr;
    const bx = x + ((nx - x) / l2) * rr;
    const by = y + ((ny - y) / l2) * rr;
    d += ` L${ax} ${ay} Q${x} ${y} ${bx} ${by}`;
  }
  const last = pts[pts.length - 1];
  return `${d} L${last[0]} ${last[1]}`;
}

function layoutWire() {
  const s = scene.getBoundingClientRect();
  const a = devA.getBoundingClientRect();
  const b = devB.getBoundingClientRect();
  wire.setAttribute('viewBox', `0 0 ${s.width} ${s.height}`);
  const stacked = getComputedStyle(devA).position !== 'absolute';
  let pts: [number, number][];
  if (stacked) {
    // Телефон: устройства друг под другом, провод сверху вниз с одной ступенькой.
    const x1 = a.left - s.left + a.width * 0.28;
    const x2 = b.left - s.left + b.width * 0.62;
    const y1 = a.bottom - s.top;
    const y2 = b.top - s.top;
    const mid = (y1 + y2) / 2;
    pts = [[x1, y1], [x1, mid], [x2, mid], [x2, y2]];
  } else {
    // Десктоп: из-под первого устройства вниз и вправо, в бок второго.
    const x1 = a.left - s.left + Math.min(a.width * 0.3, b.left - a.left - 28);
    const y1 = a.bottom - s.top;
    const x2 = b.left - s.left;
    const y2 = Math.max(b.top - s.top + 92, y1 + 40);
    pts = [[x1, y1], [x1, y2], [x2, y2]];
  }
  const d = roundedPath(pts, 18);
  wireBase.setAttribute('d', d);
  wireGlow.setAttribute('d', d);
  endA.setAttribute('cx', String(pts[0][0]));
  endA.setAttribute('cy', String(pts[0][1]));
  const e = pts[pts.length - 1];
  endB.setAttribute('cx', String(e[0]));
  endB.setAttribute('cy', String(e[1]));
}

function insertHeroRow(i: number, animate: boolean) {
  const r = data.rows[i % data.rows.length];
  const li = document.createElement('li');
  li.className = 'pw-row sel';
  li.innerHTML = rowHtml(r);
  const state = animate ? Flip.getState(Array.from(heroList.children)) : null;
  heroList.querySelectorAll('.sel').forEach((el) => el.classList.remove('sel'));
  heroList.prepend(li);
  while (heroList.children.length > 6) heroList.lastElementChild!.remove();
  if (state) {
    Flip.from(state, {
      duration: 0.5,
      ease: 'power3.out',
      onEnter: (els) => gsap.fromTo(els, { autoAlpha: 0, y: -10 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }),
    });
  }
}

const docs = $$('[data-doc]');
const k1 = $('[data-k1]');
const k2 = $('[data-k2]');
const copied = $('[data-copied]');

function showDoc(i: number) {
  docs.forEach((d) => d.classList.toggle('on', Number(d.dataset.doc) === i));
  return docs[i];
}

let heroTl: gsap.core.Timeline | null = null;
let heroVisible = true;
let heroStarted = false;

function heroCycle(i: number) {
  const n = i % docs.length;
  layoutWire();
  const doc = showDoc(n);
  const mark = doc.querySelector<HTMLElement>('.sel-mark');
  const card = doc.querySelector<HTMLElement>('.sel-card');
  const cipher = Array.from({ length: 12 }, (_, k) => HEX[(n * 7 + k * 5) % 16]).join('');

  const tl = gsap.timeline({ onComplete: () => heroCycle(i + 1), defaults: { ease: 'power2.out' } });
  tl.fromTo(doc, { autoAlpha: 0, y: 6 }, { autoAlpha: 1, y: 0, duration: 0.4 });
  if (mark) tl.fromTo(mark, { backgroundSize: '0% 100%' }, { backgroundSize: '100% 100%', duration: 0.55, ease: 'power1.inOut' }, '+=0.2');
  if (card) tl.call(() => card.classList.add('picked'), undefined, '+=0.3');
  tl.call(() => k1.classList.add('down'), undefined, '+=0.25')
    .call(() => k2.classList.add('down'), undefined, '+=0.12')
    .to(copied, { autoAlpha: 1, duration: 0.2 })
    .call(() => {
      k1.classList.remove('down');
      k2.classList.remove('down');
    }, undefined, '+=0.15')
    .addLabel('go')
    .set(packetText, { textContent: '········' }, 'go')
    .fromTo(packet, { autoAlpha: 0 }, { autoAlpha: 1, duration: 0.15 }, 'go')
    .fromTo(wireGlow, { drawSVG: '0% 0%' }, { drawSVG: '0% 100%', duration: 1.2, ease: 'power1.inOut' }, 'go')
    .to(packet, { motionPath: { path: wireBase, align: wireBase, alignOrigin: [0.5, 0.5] }, duration: 1.2, ease: 'power1.inOut' }, 'go')
    .to(packetText, { duration: 1.1, scrambleText: { text: cipher, chars: HEX, speed: 0.7 } }, 'go')
    .to(packet, { autoAlpha: 0, duration: 0.15 })
    .call(() => insertHeroRow(n, true))
    .to(wireGlow, { drawSVG: '100% 100%', duration: 0.6, ease: 'power2.inOut' }, '<')
    .to(copied, { autoAlpha: 0, duration: 0.2 }, '<')
    .to(doc, { autoAlpha: 0, duration: 0.3 }, '+=1.8')
    .call(() => {
      card?.classList.remove('picked');
      if (mark) gsap.set(mark, { backgroundSize: '0% 100%' });
    });
  heroTl = tl;
  if (!heroVisible) tl.pause();
}

function heroStatic() {
  layoutWire();
  showDoc(0);
  gsap.set(docs[0], { autoAlpha: 1 });
  gsap.set(wireGlow, { drawSVG: '0% 100%' });
  if (!heroList.querySelector('.sel')) insertHeroRow(0, false);
}

// ── Разделы по прокрутке ───────────────────────────────────────────────
function setStep(key: string) {
  $$('[data-step]').forEach((s) => s.classList.toggle('on', s.dataset.step === key));
}

function demoTimelines(narrow: boolean) {
  // Текст с оформлением: Enter и Shift+Enter по очереди.
  const rich = $('[data-demo="rich"]');
  const fmt = $('[data-rich-fmt]');
  const plain = $('[data-rich-plain]');
  const mode = $('[data-rich-mode]');
  const [kEnter, kShift] = $$('[data-rk]');
  const rtl = gsap.timeline({ repeat: -1, paused: true });
  rtl
    .set(plain, { autoAlpha: 0 })
    .set(fmt, { autoAlpha: 1 })
    .call(() => {
      kEnter.classList.add('on');
      kShift.classList.remove('on');
      mode.textContent = mode.dataset.a!;
    })
    .addLabel('flip', '+=2.4')
    .call(() => {
      kShift.classList.add('on');
      kEnter.classList.remove('on');
      mode.textContent = mode.dataset.b!;
    }, undefined, 'flip')
    .to(fmt, { autoAlpha: 0, duration: 0.25 }, 'flip')
    .to(plain, { autoAlpha: 1, duration: 0.25 }, 'flip')
    .to({}, { duration: 2.4 })
    .to(plain, { autoAlpha: 0, duration: 0.25 })
    .to(fmt, { autoAlpha: 1, duration: 0.25 }, '<');
  ScrollTrigger.create({ trigger: rich, start: 'top 85%', end: 'bottom 15%', onToggle: (s) => (s.isActive ? rtl.play() : rtl.pause()) });

  // Картинка: один раз проявляется.
  gsap.from('[data-demo="image"]', {
    autoAlpha: 0,
    y: 24,
    duration: 0.7,
    ease: 'power3.out',
    scrollTrigger: { trigger: '[data-demo="image"]', start: 'top 85%', once: true },
  });

  // Файл: карточка сразу, сам файл в момент вставки.
  const files = $('[data-demo="files"]');
  const fcard = $('[data-fcard]');
  const fstate = $('[data-fstate]');
  const fbar = $('[data-fbar]');
  const fkey = $('[data-fkey]');
  const beam = $('[data-files-beam]');
  const axis = narrow ? 'scaleY' : 'scaleX';
  const ftl = gsap.timeline({ repeat: -1, repeatDelay: 0.8, paused: true });
  ftl
    .set(fcard, { autoAlpha: 0, y: 8 })
    .set(fbar, { scaleX: 0 })
    .set(beam, { [axis]: 0, transformOrigin: narrow ? 'center top' : 'left center' })
    .call(() => (fstate.textContent = fstate.dataset.w!))
    .to(beam, { [axis]: 1, duration: 0.35, ease: 'power2.out' }, 0.3)
    .to(fcard, { autoAlpha: 1, y: 0, duration: 0.4, ease: 'power3.out' }, '>-0.05')
    .to(beam, { autoAlpha: 0, duration: 0.3 }, '+=0.2')
    .call(() => fkey.classList.add('down'), undefined, '+=1.1')
    .call(() => {
      fkey.classList.remove('down');
      fstate.textContent = fstate.dataset.p!;
    }, undefined, '+=0.18')
    .set(beam, { autoAlpha: 1, [axis]: 0 })
    .to(beam, { [axis]: 1, duration: 0.35 })
    .to(fbar, { scaleX: 1, duration: 1.8, ease: 'none' }, '<')
    .call(() => (fstate.textContent = fstate.dataset.d!))
    .to(beam, { autoAlpha: 0, duration: 0.3 }, '<')
    .to(fcard, { autoAlpha: 0, duration: 0.35 }, '+=1.8')
    .set(beam, { autoAlpha: 1 });
  ScrollTrigger.create({ trigger: files, start: 'top 85%', end: 'bottom 15%', onToggle: (s) => (s.isActive ? ftl.play() : ftl.pause()) });

  // Ссылки: пробел открывает карточку, одноразовая ссылка не грузится.
  const links = $('[data-demo="links"]');
  const lcard = $('[data-lcard]');
  const lkey = $('[data-lkey]');
  const lbad = $('[data-lbad]');
  const ltl = gsap.timeline({ repeat: -1, repeatDelay: 0.6, paused: true });
  ltl
    .set([lcard, lbad], { autoAlpha: 0, y: 8 })
    .call(() => lkey.classList.add('down'), undefined, 0.6)
    .call(() => lkey.classList.remove('down'), undefined, 0.78)
    .to(lcard, { autoAlpha: 1, y: 0, duration: 0.4, ease: 'power3.out' }, 0.85)
    .to(lbad, { autoAlpha: 1, y: 0, duration: 0.4, ease: 'power3.out' }, '+=1.2')
    .to([lcard, lbad], { autoAlpha: 0, duration: 0.35 }, '+=2.6');
  ScrollTrigger.create({ trigger: links, start: 'top 85%', end: 'bottom 15%', onToggle: (s) => (s.isActive ? ltl.play() : ltl.pause()) });
}

function stepTriggers() {
  $$('[data-panel]').forEach((p) =>
    ScrollTrigger.create({
      trigger: p,
      start: 'top 65%',
      end: 'bottom 65%',
      onToggle: (s) => s.isActive && setStep(p.dataset.panel!),
    }),
  );
}

// Шифрование: секрет уходит, на сервере шум, на втором устройстве снова секрет.
function cryptTimeline(pinIt: boolean) {
  const section = $('#security');
  const cs = $('[data-cv="s"]');
  const cb = $('[data-cv="b"]');
  const secret = cs.dataset.secret!;
  const [l1, l2] = $$('[data-cl] i');
  const axis = pinIt ? 'scaleX' : getComputedStyle(l1.parentElement!).width === '3px' ? 'scaleY' : 'scaleX';
  const tl = gsap.timeline({
    defaults: { ease: 'none' },
    scrollTrigger: pinIt
      ? { trigger: section, start: 'top top+=72', end: '+=900', pin: true, scrub: 0.6 }
      : { trigger: '[data-crypt]', start: 'top 75%', once: true },
  });
  tl.set(cs, { textContent: '········' })
    .set(cb, { textContent: '········' })
    .to(l1, { [axis]: 1, duration: 1 })
    .to(cs, { duration: 1.4, scrambleText: { text: CIPHER, chars: CIPHER, speed: 0.5 } })
    .to(l2, { [axis]: 1, duration: 1 })
    .to(cb, { duration: 1.4, scrambleText: { text: secret, chars: CIPHER, speed: 0.5 } });
  if (!pinIt) tl.duration(3.2);
}

// Исключения: поток копирований, лишнее отсекается у ворот.
let laneTl: gsap.core.Timeline | null = null;
function buildLane() {
  laneTl?.kill();
  const lane = $('[data-lane]');
  const gate = $('[data-gate]');
  const chips = $$('[data-chip]', lane);
  const laneW = lane.clientWidth;
  const gateX = gate.offsetLeft;
  const v = laneW < 600 ? 120 : 190; // пикселей в секунду
  const tl = gsap.timeline({ repeat: -1, paused: true });
  let t = 0;
  chips.forEach((chip) => {
    const w = chip.offsetWidth;
    const pass = chip.dataset.pass === 'true';
    const toGate = (gateX - 8 + w + 12) / v;
    tl.set(chip, { x: -w - 12, y: 0, autoAlpha: 1 }, t)
      .call(() => chip.classList.remove('blocked', 'through'), undefined, t)
      .to(chip, { x: gateX - w - 8, duration: toGate, ease: 'none' }, t);
    if (pass) {
      tl.call(() => chip.classList.add('through'), undefined, t + toGate).to(
        chip,
        { x: laneW + 12, duration: (laneW - gateX + w + 20) / v, ease: 'none' },
        t + toGate,
      );
      t += (w + 56) / v;
    } else {
      tl.call(() => chip.classList.add('blocked'), undefined, t + toGate).to(
        chip,
        { y: 52, autoAlpha: 0, duration: 0.5, ease: 'power2.in' },
        t + toGate + 0.7,
      );
      t += (w + 56) / v + 0.9;
    }
  });
  tl.to({}, { duration: 0.6 });
  laneTl = tl;
  ScrollTrigger.create({ trigger: lane, start: 'top 90%', end: 'bottom 10%', onToggle: (s) => (s.isActive ? tl.play() : tl.pause()) });
}

function laneStatic() {
  const lane = $('[data-lane]');
  const gate = $('[data-gate]');
  const chips = $$('[data-chip]', lane);
  const gateX = gate.offsetLeft;
  // Готовый кадр: два файла прошли, один отсечён у ворот.
  const pass = chips.filter((c) => c.dataset.pass === 'true').slice(0, 2);
  const blocked = chips.find((c) => c.dataset.pass === 'false')!;
  gsap.set(pass[0], { x: lane.clientWidth - pass[0].offsetWidth - 24, autoAlpha: 1 });
  gsap.set(pass[1], { x: gateX + 24, autoAlpha: 1, y: -44 });
  pass.forEach((c) => c.classList.add('through'));
  blocked.classList.add('blocked');
  gsap.set(blocked, { x: gateX - blocked.offsetWidth - 8, autoAlpha: 1 });
}

// ── Окно вставки, которым можно управлять ──────────────────────────────
function initTry() {
  const root = $('[data-try]');
  const input = $<HTMLInputElement>('[data-pw-input]', root);
  const list = $('[data-pw-list]', root);
  const preview = $('[data-try-preview]', root);
  const doc = $('[data-try-doc]', root);
  const rows = data.tryRows;
  let shown = rows.map((_, i) => i);
  let sel = 0;
  let previewOn = false;

  const render = () => {
    if (!shown.length) {
      list.innerHTML = `<li class="try-empty-list">${esc(data.noResults)}</li>`;
    } else {
      list.innerHTML = shown
        .map(
          (ri, k) =>
            `<li class="pw-row${k === sel ? ' sel' : ''}" role="option" aria-selected="${k === sel}" data-k="${k}">${rowHtml(rows[ri], k)}</li>`,
        )
        .join('');
    }
    renderPreview();
  };
  const renderPreview = () => {
    preview.hidden = !previewOn || !shown.length;
    if (preview.hidden) return;
    const r = rows[shown[sel]];
    let body: string;
    if (r.kind === 'image')
      body = `<span class="pw-thumb"><span class="mini-chart" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i><i></i><i></i></span></span>`;
    else if (r.kind === 'file' || r.kind === 'folder')
      body = `<div class="fcard"><span class="file-ic">${icon(r.kind)}</span><span class="fcard-body"><b>${esc(r.text)}</b></span></div>`;
    else body = `<div class="pv-text${r.kind === 'code' ? ' mono' : ''}">${esc(r.text)}</div>`;
    preview.innerHTML = `${body}<div class="pv-meta">${esc(r.meta)}</div>`;
  };
  const select = (k: number) => {
    if (!shown.length) return;
    sel = Math.max(0, Math.min(shown.length - 1, k));
    $$('.pw-row', list).forEach((li, i) => {
      li.classList.toggle('sel', i === sel);
      li.setAttribute('aria-selected', String(i === sel));
    });
    $$('.pw-row', list)[sel]?.scrollIntoView({ block: 'nearest' });
    renderPreview();
  };
  const paste = () => {
    if (!shown.length) return;
    const r = rows[shown[sel]];
    doc.querySelector('.try-empty')?.remove();
    const line = document.createElement('div');
    if (r.kind === 'image') line.innerHTML = `<span class="pw-thumb"><span class="mini-chart" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i><i></i><i></i></span></span>`;
    else if (r.kind === 'file' || r.kind === 'folder') line.innerHTML = `<span class="mono">${esc(r.text)}</span>`;
    else {
      line.textContent = r.text;
      if (r.kind === 'code') line.className = 'mono';
    }
    doc.append(line);
    while (doc.children.length > 5) doc.firstElementChild!.remove();
    if (!reduceQuery.matches) gsap.fromTo(line, { autoAlpha: 0, y: 6 }, { autoAlpha: 1, y: 0, duration: 0.25, ease: 'power3.out' });
  };

  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      select(sel + 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      select(sel - 1);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      paste();
    } else if (e.key === ' ' && input.value === '') {
      e.preventDefault();
      previewOn = !previewOn;
      renderPreview();
    } else if (e.key === 'Escape') {
      if (previewOn) previewOn = false;
      else input.value = '';
      filter();
    }
  });
  const filter = () => {
    const q = input.value.trim().toLowerCase();
    shown = rows.map((_, i) => i).filter((i) => !q || `${rows[i].text} ${rows[i].meta}`.toLowerCase().includes(q));
    sel = 0;
    render();
  };
  input.addEventListener('input', filter);
  list.addEventListener('click', (e) => {
    const li = (e.target as Element).closest<HTMLElement>('.pw-row');
    if (!li) return;
    const k = Number(li.dataset.k);
    // Тап по уже выбранной строке вставляет: на телефоне нет Enter.
    if (k === sel && e.detail < 2) paste();
    else select(k);
    input.focus({ preventScroll: true });
  });
  list.addEventListener('dblclick', (e) => {
    if ((e.target as Element).closest('.pw-row')) paste();
  });
  render();
}

// ── Запуск ─────────────────────────────────────────────────────────────
initTry();

const mm = gsap.matchMedia();
mm.add(
  {
    motion: '(prefers-reduced-motion: no-preference)',
    reduce: '(prefers-reduced-motion: reduce)',
    wide: '(min-width: 1024px)',
    narrow: '(max-width: 719px)',
  },
  (ctx) => {
    const { motion, wide, narrow } = ctx.conditions as Record<string, boolean>;
    stepTriggers();
    if (!motion) {
      heroStatic();
      laneStatic();
      gsap.set('[data-cl] i', { scaleX: 1, scaleY: 1 });
      $('[data-cv="s"]').textContent = CIPHER;
      $('[data-cv="b"]').textContent = $('[data-cv="s"]').dataset.secret!;
      return;
    }
    // Сцена главного экрана: стоит, пока её не видно.
    if (!heroStarted) {
      heroStarted = true;
      heroCycle(0);
    }
    ScrollTrigger.create({
      trigger: scene,
      start: 'top bottom',
      end: 'bottom top',
      onToggle: (s) => {
        heroVisible = s.isActive;
        if (s.isActive) heroTl?.resume();
        else heroTl?.pause();
      },
    });
    demoTimelines(narrow);
    cryptTimeline(wide);
    buildLane();
    gsap.from('[data-code] .cl', {
      autoAlpha: 0,
      x: -6,
      duration: 0.35,
      stagger: 0.05,
      ease: 'power2.out',
      scrollTrigger: { trigger: '[data-code]', start: 'top 80%', once: true },
    });
    return () => {
      laneTl?.kill();
    };
  },
);

// Провод и дорожка зависят от размеров: пересчитываем после шрифтов и при ресайзе.
let resizeT = 0;
window.addEventListener('resize', () => {
  clearTimeout(resizeT);
  resizeT = window.setTimeout(() => {
    layoutWire();
    if (!reduceQuery.matches) buildLane();
  }, 200);
});
document.fonts?.ready.then(() => {
  layoutWire();
  ScrollTrigger.refresh();
});

// Для проверки в разработке: доступ к таймлайну сцены из консоли.
if (import.meta.env.DEV) (window as unknown as { __cw: object }).__cw = { hero: () => heroTl, lane: () => laneTl };
