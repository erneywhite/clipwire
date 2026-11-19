// Живые части страницы: сцена на главном экране, разделы по прокрутке,
// окно вставки, которым можно управлять, и кнопки скачивания под систему.
import { gsap } from 'gsap';
import { ScrollTrigger } from 'gsap/ScrollTrigger';
import { MotionPathPlugin } from 'gsap/MotionPathPlugin';
import { DrawSVGPlugin } from 'gsap/DrawSVGPlugin';
import { ScrambleTextPlugin } from 'gsap/ScrambleTextPlugin';
import { Flip } from 'gsap/Flip';
import { SplitText } from 'gsap/SplitText';

gsap.registerPlugin(ScrollTrigger, MotionPathPlugin, DrawSVGPlugin, ScrambleTextPlugin, Flip, SplitText);

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

// ── Главный экран: два устройства копируют в одну общую историю ────────
type Side = 'a' | 'b';
const scene = $('[data-scene]');
const devs: Record<Side, HTMLElement> = { a: $('[data-dev="a"]'), b: $('[data-dev="b"]') };
const devPw = $('[data-dev-pw]');
const wire = $<SVGSVGElement>('[data-wire]');
const wires = Object.fromEntries(
  (['a', 'b'] as Side[]).map((side) => {
    const g = $(`[data-wire-g="${side}"]`);
    return [side, { base: $<SVGPathElement>('[data-base]', g), glow: $<SVGPathElement>('[data-glow]', g), e1: $('[data-end1]', g), e2: $('[data-end2]', g) }];
  }),
) as Record<Side, { base: SVGPathElement; glow: SVGPathElement; e1: Element; e2: Element }>;
const packet = $('[data-packet]');
const packetText = $('[data-packet-text]');
const heroList = $('#hero-pw [data-pw-list]');
const sideOf = (n: number) => (data as unknown as { from: Side[] }).from[n];

/** Ломаная с закруглёнными углами, как провод на знаке. */
function roundedPath(pts: [number, number][], r = 16) {
  let d = `M${pts[0][0]} ${pts[0][1]}`;
  for (let i = 1; i < pts.length - 1; i++) {
    const [px, py] = pts[i - 1];
    const [x, y] = pts[i];
    const [nx, ny] = pts[i + 1];
    const l1 = Math.hypot(x - px, y - py) || 1;
    const l2 = Math.hypot(nx - x, ny - y) || 1;
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
  const pw = devPw.getBoundingClientRect();
  wire.setAttribute('viewBox', `0 0 ${s.width} ${s.height}`);
  const stacked = getComputedStyle(devs.a).position !== 'absolute';
  const X = (r: DOMRect, f: number) => r.left - s.left + r.width * f;
  (['a', 'b'] as Side[]).forEach((side) => {
    const r = devs[side].getBoundingClientRect();
    let x1: number, y1: number, x2: number, y2: number;
    if (!stacked) {
      // Десктоп: оба устройства сверху, провода сходятся в верх общей истории.
      x1 = X(r, side === 'a' ? 0.32 : 0.68);
      y1 = r.bottom - s.top;
      x2 = X(pw, side === 'a' ? 0.3 : 0.7);
      y2 = pw.top - s.top;
    } else if (side === 'a') {
      // Телефон: work-pc над историей, macbook под ней.
      x1 = X(r, 0.5);
      y1 = r.bottom - s.top;
      x2 = X(pw, 0.84);
      y2 = pw.top - s.top;
    } else {
      x1 = X(r, 0.5);
      y1 = r.top - s.top;
      x2 = X(pw, 0.84);
      y2 = pw.bottom - s.top;
    }
    const mid = (y1 + y2) / 2;
    const d = roundedPath([[x1, y1], [x1, mid], [x2, mid], [x2, y2]], 18);
    const w = wires[side];
    w.base.setAttribute('d', d);
    w.glow.setAttribute('d', d);
    w.e1.setAttribute('cx', String(x1));
    w.e1.setAttribute('cy', String(y1));
    w.e2.setAttribute('cx', String(x2));
    w.e2.setAttribute('cy', String(y2));
  });
}

function insertHeroRow(n: number, animate: boolean) {
  const r = data.rows[n];
  const li = document.createElement('li');
  li.className = 'pw-row sel';
  li.innerHTML = rowHtml(r);
  const state = animate ? Flip.getState(Array.from(heroList.children)) : null;
  heroList.querySelectorAll('.sel').forEach((el) => el.classList.remove('sel'));
  heroList.prepend(li);
  while (heroList.children.length > 6) heroList.lastElementChild!.remove();
  if (state) {
    Flip.from(state, {
      duration: 0.6,
      ease: 'power3.out',
      onEnter: (els) => gsap.fromTo(els, { autoAlpha: 0, y: -10 }, { autoAlpha: 1, y: 0, duration: 0.5, ease: 'power3.out' }),
    });
  }
}

const docOf = (n: number) => $(`[data-doc="${n}"]`);
const currentDoc = (side: Side) => $('.doc.on', devs[side]);

let heroTl: gsap.core.Timeline | null = null;
let heroVisible = true;
let heroStarted = false;

function heroCycle(i: number) {
  const n = i % data.rows.length;
  const side = sideOf(n);
  const dev = devs[side];
  const w = wires[side];
  layoutWire();
  const prev = currentDoc(side);
  const doc = docOf(n);
  const mark = doc.querySelector<HTMLElement>('.sel-mark');
  const card = doc.querySelector<HTMLElement>('.sel-card');
  const k1 = $('[data-k1]', dev);
  const k2 = $('[data-k2]', dev);
  const copied = $('[data-copied]', dev);
  const cipher = Array.from({ length: 12 }, (_, k) => HEX[(n * 7 + k * 5) % 16]).join('');

  const tl = gsap.timeline({ onComplete: () => heroCycle(i + 1), defaults: { ease: 'power2.out' } });
  if (prev !== doc) {
    tl.to(prev, { autoAlpha: 0, duration: 0.3 })
      .call(() => {
        prev.classList.remove('on');
        doc.classList.add('on');
      })
      .fromTo(doc, { autoAlpha: 0, y: 6 }, { autoAlpha: 1, y: 0, duration: 0.4 });
  }
  if (mark) tl.fromTo(mark, { backgroundSize: '0% 100%' }, { backgroundSize: '100% 100%', duration: 0.6, ease: 'power1.inOut' }, '+=0.25');
  if (card) tl.call(() => card.classList.add('picked'), undefined, '+=0.35');
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
    .fromTo(w.glow, { drawSVG: '0% 0%' }, { drawSVG: '0% 100%', duration: 1.3, ease: 'power1.inOut' }, 'go')
    .to(packet, { motionPath: { path: w.base, align: w.base, alignOrigin: [0.5, 0.5] }, duration: 1.3, ease: 'power1.inOut' }, 'go')
    .to(packetText, { duration: 1.2, scrambleText: { text: cipher, chars: HEX, speed: 0.7 } }, 'go')
    .to(packet, { autoAlpha: 0, duration: 0.15 })
    .call(() => insertHeroRow(n, true))
    .to(w.glow, { drawSVG: '100% 100%', duration: 0.6, ease: 'power2.inOut' }, '<')
    .to(copied, { autoAlpha: 0, duration: 0.25 }, '<')
    .addLabel('rest', '+=1.4');
  // Выделение гаснет: устройство ждёт своей очереди с обычным текстом.
  if (mark) tl.to(mark, { backgroundSize: '0% 100%', duration: 0.4, ease: 'power1.inOut' }, 'rest');
  if (card) tl.call(() => card.classList.remove('picked'), undefined, 'rest');
  tl.to({}, { duration: 0.35 });
  heroTl = tl;
  if (!heroVisible) tl.pause();
}

function heroStatic() {
  layoutWire();
  gsap.set(wires.a.glow, { drawSVG: '0% 100%' });
  if (!heroList.querySelector('.sel')) {
    insertHeroRow(1, false);
    insertHeroRow(0, false);
  }
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
    .set(plain, { autoAlpha: 0, filter: 'blur(4px)' })
    .set(fmt, { autoAlpha: 1, filter: 'blur(0px)' })
    .call(() => {
      kEnter.classList.add('on');
      kShift.classList.remove('on');
      mode.textContent = mode.dataset.a!;
    })
    .addLabel('flip', '+=2.6')
    .call(() => {
      kShift.classList.add('on');
      kEnter.classList.remove('on');
      mode.textContent = mode.dataset.b!;
    }, undefined, 'flip')
    .to(fmt, { autoAlpha: 0, filter: 'blur(4px)', duration: 0.45, ease: 'power2.inOut' }, 'flip')
    .to(plain, { autoAlpha: 1, filter: 'blur(0px)', duration: 0.5, ease: 'power2.inOut' }, 'flip+=0.3')
    .addLabel('back', '+=2.6')
    .call(() => {
      kEnter.classList.add('on');
      kShift.classList.remove('on');
      mode.textContent = mode.dataset.a!;
    }, undefined, 'back')
    .to(plain, { autoAlpha: 0, filter: 'blur(4px)', duration: 0.45, ease: 'power2.inOut' }, 'back')
    .to(fmt, { autoAlpha: 1, filter: 'blur(0px)', duration: 0.5, ease: 'power2.inOut' }, 'back+=0.3');
  ScrollTrigger.create({ trigger: rich, start: 'top 85%', end: 'bottom 15%', onToggle: (s) => (s.isActive ? rtl.play() : rtl.pause()) });

  // Каждая демонстрация плавно проявляется вместе с прокруткой.
  $$('[data-panel] .demo').forEach((d) =>
    gsap.fromTo(
      d,
      { autoAlpha: 0.15, y: 56, scale: 0.97 },
      { autoAlpha: 1, y: 0, scale: 1, ease: 'none', scrollTrigger: { trigger: d, start: 'top 95%', end: 'top 50%', scrub: 0.8 } },
    ),
  );

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
    .to(beam, { [axis]: 1, duration: 0.55, ease: 'power2.inOut' }, 0.3)
    .to(fcard, { autoAlpha: 1, y: 0, duration: 0.6, ease: 'power3.out' }, '>-0.1')
    .to(beam, { autoAlpha: 0, duration: 0.45 }, '+=0.2')
    .call(() => fkey.classList.add('down'), undefined, '+=1.1')
    .call(() => {
      fkey.classList.remove('down');
      fstate.textContent = fstate.dataset.p!;
    }, undefined, '+=0.18')
    .set(beam, { autoAlpha: 1, [axis]: 0 })
    .to(beam, { [axis]: 1, duration: 0.5, ease: 'power2.inOut' })
    .to(fbar, { scaleX: 1, duration: 2.2, ease: 'power1.inOut' }, '<')
    .call(() => (fstate.textContent = fstate.dataset.d!))
    .to(beam, { autoAlpha: 0, duration: 0.45 }, '<')
    .to(fcard, { autoAlpha: 0, y: -6, duration: 0.5, ease: 'power2.inOut' }, '+=1.8')
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
    .to(lcard, { autoAlpha: 1, y: 0, duration: 0.6, ease: 'power3.out' }, 0.85)
    .to(lbad, { autoAlpha: 1, y: 0, duration: 0.6, ease: 'power3.out' }, '+=1.2')
    .to([lcard, lbad], { autoAlpha: 0, y: -6, duration: 0.5, ease: 'power2.inOut', stagger: 0.1 }, '+=2.6');
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
// Шум не стоит на месте: пока блок на экране, символы всё время перешифровываются.
const NOISE = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const randChar = () => NOISE[(Math.random() * NOISE.length) | 0];

function cryptTimeline(pinIt: boolean, still: boolean) {
  const section = $('#security');
  const cs = $('[data-cv="s"]');
  const cb = $('[data-cv="b"]');
  const secret = cs.dataset.secret!;
  const [l1, l2] = $$('[data-cl] i');
  const points = $$('#security .points li');
  const axis = getComputedStyle(l1.parentElement!).width === '3px' ? 'scaleY' : 'scaleX';
  const N = CIPHER.length;
  const L = secret.length;
  const noise = Array.from({ length: N }, randChar);
  const st = { l1: 0, fill: 0, l2: 0, dec: 0 };
  // Весь шум перешифровывается каждый тик: выглядит как непрерывное шифрование.
  const mutate = () => {
    for (let i = 0; i < N; i++) noise[i] = randChar();
  };
  const render = () => {
    gsap.set(l1, { [axis]: st.l1 });
    gsap.set(l2, { [axis]: st.l2 });
    const nf = Math.round(N * st.fill);
    cs.textContent = nf ? noise.slice(0, nf).join('') : '········';
    const nd = Math.round(L * st.dec);
    cb.textContent = st.dec > 0 ? secret.slice(0, nd) + noise.slice(0, L - nd).join('') : '········';
  };

  if (still) {
    Object.assign(st, { l1: 1, fill: 1, l2: 1, dec: 1 });
    render();
    return;
  }

  // Заголовок и подзаголовок проявляются по словам вместе с прокруткой.
  const split = SplitText.create(['#security .sec-h', '#security .sec-lead'], { type: 'words' });
  gsap.fromTo(
    split.words,
    { autoAlpha: 0.12, y: 10 },
    {
      autoAlpha: 1,
      y: 0,
      stagger: 0.05,
      ease: 'none',
      scrollTrigger: { trigger: '#security .sec-head', start: 'top 90%', end: 'top 40%', scrub: 0.8 },
    },
  );

  const tl = gsap.timeline({
    defaults: { ease: 'none' },
    onUpdate: render,
    scrollTrigger: pinIt
      ? { trigger: section, start: 'top top+=72', end: '+=1100', pin: true, scrub: 0.8 }
      : { trigger: '[data-crypt]', start: 'top 75%', once: true },
  });
  tl.to(st, { l1: 1, duration: 1 })
    .to(st, { fill: 1, duration: 1.2 })
    .to(st, { l2: 1, duration: 1 })
    .to(st, { dec: 1, duration: 1.6 })
    .fromTo(points, { autoAlpha: 0.15, y: 16 }, { autoAlpha: 1, y: 0, stagger: 0.25, duration: 0.8, ease: 'power1.out' }, '-=1.2');
  if (!pinIt) tl.duration(4);
  render();

  // Мелькает и без прокрутки: таймер работает, пока блок виден на экране.
  let timer = 0;
  new IntersectionObserver(([e]) => {
    clearInterval(timer);
    if (e.isIntersecting)
      timer = window.setInterval(() => {
        mutate();
        render();
      }, 70);
  }).observe(section);
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
  const v = Math.min(300, Math.max(120, laneW * 0.2)); // пикселей в секунду, от ширины дорожки
  const tl = gsap.timeline({ repeat: -1 });
  let t = 0;
  chips.forEach((chip) => {
    const w = chip.offsetWidth;
    const pass = chip.dataset.pass === 'true';
    // Путь до ворот у всех фишек одинаковый (gateX + 4), значит и время одно: скорость общая.
    const toGate = (gateX + 4) / v;
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

// Без сервера: переключатель уезжает на «Всегда», а история бесконечно тянется в прошлое.
function localScene() {
  const thumb = $('[data-seg-thumb]');
  const opts = $$('[data-seg-o]');
  const last = opts.length - 1;
  const mark = (i: number) => opts.forEach((o, k) => o.classList.toggle('on', k === i));
  gsap.set(thumb, { left: 4, x: 0, y: 0, xPercent: 0 });
  mark(0);
  const seg = gsap.timeline({ scrollTrigger: { trigger: '[data-local]', start: 'top 75%', once: true } });
  opts.forEach((_, i) => {
    if (!i) return;
    seg.to(thumb, { xPercent: i * 100, duration: 0.32, ease: 'power2.inOut', onStart: () => mark(i) }, i === 1 ? 0.2 : '+=0.12');
  });
  seg.call(() => mark(last));
  // Список из двух одинаковых половин: сдвиг ровно на половину даёт бесшовный круг.
  gsap.to('[data-local-track]', { yPercent: -50, duration: 40, ease: 'none', repeat: -1 });
}

// Копирование compose-файла.
$$<HTMLButtonElement>('[data-copy]').forEach((btn) => {
  const label = $('span', btn);
  const idle = label.textContent;
  btn.addEventListener('click', async () => {
    const text = $('pre', btn.closest('figure')!).textContent!.replace(/\n+$/, '\n');
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.append(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    label.textContent = btn.dataset.ok!;
    btn.classList.add('done');
    window.setTimeout(() => {
      label.textContent = idle;
      btn.classList.remove('done');
    }, 1800);
  });
});

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
      cryptTimeline(false, true);
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
    cryptTimeline(wide, false);
    buildLane();
    localScene();
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
