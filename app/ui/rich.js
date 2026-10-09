// What a model writes, drawn as a page draws it: Markdown, highlighted code, Mermaid diagrams,
// Vega-Lite charts and HTML previews, in a message or a file opened beside. Markdown is parsed once
// per message (marked, loaded with the page); highlighting, Mermaid and Vega load the first time
// something needs them. Raw HTML in Markdown stays text; an ```html block runs only when asked, in a
// sandboxed frame with no network and no way into the app; a chart loads no data from anywhere.
window.Rich = (() => {
  'use strict';
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const linkable = (href) => /^(https?:|mailto:)/i.test(href ?? '');
  // Bumped when highlighting arrives, so HTML drawn without it is drawn again (see `ready`).
  // `waited` says the last `html` drew code plain while highlighting loads, so only such a
  // message needs drawing again once it arrives.
  let version = 0, ready = () => {}, waited = false;

  // ---------- lazy scripts ----------
  const loading = new Map();
  function script(src) {
    if (!loading.has(src)) loading.set(src, new Promise((resolve, reject) => {
      const s = document.createElement('script'); s.src = src; s.async = true;
      s.onload = resolve; s.onerror = () => { loading.delete(src); reject(new Error(`${src} did not load`)); };
      (document.head ?? document.body)?.append?.(s);
    }));
    return loading.get(src);
  }
  const hl = () => globalThis.hljs ?? null;
  function wantHighlight() {
    if (hl() || loading.has('vendor/highlight.js')) return;
    script('vendor/highlight.js').then(() => { version++; ready(); }, () => {});
  }

  // ---------- Markdown ----------
  // Past this a block is shown as plain text: highlighting is linear but not free.
  // A message or file highlights at most 256 KiB of code in all (`spent`, reset by `html` and
  // `file`); past that its blocks are plain, so a file of many fences costs no more than one.
  const HIGHLIGHT_MAX = 64 * 1024, HIGHLIGHT_TOTAL = 256 * 1024;
  let spent = 0;
  function codeHTML(text, lang) {
    const h = hl();
    if (lang && text.length <= HIGHLIGHT_MAX && spent + text.length <= HIGHLIGHT_TOTAL) {
      if (h?.getLanguage(lang)) { spent += text.length; try { return h.highlight(text, { language: lang, ignoreIllegals: true }).value; } catch (_) {} }
      else if (!h) { waited = true; wantHighlight(); }
    }
    return esc(text);
  }
  const head = (lang, acts) => `<div class="rh"><span class="lang">${esc(lang)}</span><span class="ra">${acts}<button type="button" data-rich="copy">copy</button></span></div>`;
  const pre = (text, lang) => `<pre class="code"><code>${codeHTML(text, lang)}</code></pre>`;
  const CHART = new Set(['vega-lite', 'vegalite', 'vl', 'vega']);
  const toggle = '<button type="button" data-rich="view"></button>';
  // A diagram, chart, page or SVG in a message opens as code, drawn a click away. All draw on the
  // window's thread, and a few characters can ask for more than it can do (a page's loop, an SVG's
  // filters, a chart's `sequence` to a billion, a Mermaid `space:500000`), so each runs only when
  // asked, block by block. `page` draws it at once, for a file someone opened.
  function block(text, info, page = false) {
    const lang = (info ?? '').trim().split(/\s+/)[0].toLowerCase();
    if (lang === 'mermaid' || lang === 'mmd') return `<div class="rc" data-kind="mermaid" data-lazy${page ? ' data-run' : ''} data-view="code">${head('mermaid', toggle)}<div class="view"></div>${pre(text, '')}</div>`;
    if (CHART.has(lang)) return `<div class="rc" data-kind="chart" data-lang="${lang === 'vega' ? 'vega' : 'vega-lite'}" data-lazy${page ? ' data-run' : ''} data-view="code">${head(lang, toggle)}<div class="view"></div>${pre(text, 'json')}</div>`;
    if (lang === 'html' || lang === 'htm') return `<div class="rc" data-kind="html" data-view="${page ? 'view' : 'code'}">${head(lang, '<button type="button" data-rich="view"></button>')}<div class="view frame"></div>${pre(text, 'xml')}</div>`;
    // An SVG draws as an image, which runs no script and loads nothing.
    if (lang === 'svg' && /^\s*(<\?xml[^>]*>\s*)?<svg\b/i.test(text)) return `<div class="rc" data-kind="svg" data-view="${page ? 'view' : 'code'}">${head(lang, '<button type="button" data-rich="view"></button>')}<div class="view"></div>${pre(text, 'xml')}</div>`;
    return `<div class="rc" data-kind="code">${head(lang, '')}${pre(text, lang)}</div>`;
  }
  let md = null;
  function parser() {
    if (md || !globalThis.marked) return md;
    md = new globalThis.marked.Marked({
      gfm: true, breaks: true,
      renderer: {
        html: ({ text }) => esc(text),
        code: ({ text, lang }) => block(text, lang),
        // A table past 256 columns or 10,000 cells shows as its source: a short row is padded to
        // the header's width, so a few bytes a row can ask for millions of cells.
        table(token) { return token.header.length > COLUMNS || token.header.length * (token.rows.length + 1) > CELLS ? block(token.raw.replace(/\n+$/, ''), '') : false; },
        // A link to a path opens that file beside, from the agent's folder.
        link({ href, title, tokens }) { const inner = this.parser.parseInline(tokens), t = title ? ` title="${esc(title)}"` : ''; return linkable(href) ? `<a href="${esc(href)}"${t}>${inner}</a>` : filePath(href) ? `<a class="file" data-file="${esc(filePath(href))}"${t}>${inner}</a>` : inner; },
        // An image draws only from data the message carries, and only on a click: a small PNG can
        // decode to hundreds of megabytes and an animated one takes CPU for as long as it shows.
        // A remote image is a link and a local one opens beside, so drawing a message fetches
        // nothing a model chose.
        image: ({ href, text }) => /^data:image\/(png|gif|jpe?g|webp)[;,]/i.test(href ?? '') ? `<button type="button" class="img" data-img="${esc(href)}" title="${esc(text)}">image${text ? `: ${esc(text)}` : ''}</button>` : linkable(href) ? `<a href="${esc(href)}">${esc(text || href)}</a>` : filePath(href) ? `<a class="file" data-file="${esc(filePath(href))}">${esc(text || href)}</a>` : esc(text),
      },
    });
    return md;
  }
  // A link's target as a path, without its fragment (`a.md#install`, `a.rs#L12`) or a line suffix
  // (`a.rs:12`); null for a URL or anchor. A `#` or `:` in a file's name is written `%23` or `%3A`.
  function filePath(href) {
    let p = (href ?? '').replace(/#.*$/s, '').replace(/^file:\/\//i, '');
    if (!p || /^[a-z][a-z0-9+.-]*:/i.test(p)) return null;
    p = p.replace(/:\d+(:\d+)?$/, ''); try { p = decodeURIComponent(p); } catch (_) {}
    return p || null;
  }
  // A message's HTML, inside the caller's `.md` box. One that would draw past 100,000 tags (about
  // 50,000 elements) shows as its text: a line of `- x` or a `*x*` makes an element from a few
  // bytes, and the window pays for every element it holds. Past 50,000 lines it is not parsed.
  const TAGS = 100000, LINES = 50000;
  const count = (s, c, max) => { let n = 0, i = -1; while (n <= max && (i = s.indexOf(c, i + 1)) !== -1) n++; return n; };
  const asText = (text) => `<div class="rc" data-kind="code">${head('text', '')}<pre class="code"><code>${esc(text)}</code></pre></div>`;
  // `used` carries the bounds across the pieces of one message drawn apart, as a streamed reply's
  // blocks are; once over, `used.over` is set and that piece is text.
  function html(text, used = { lines: 0, tags: 0 }) {
    waited = false; spent = 0;
    const p = parser();
    if (!p) return `<p>${esc(text)}</p>`;
    const lines = count(text, '\n', LINES - used.lines);
    if (used.lines + lines > LINES) { used.over = true; return asText(text); }
    let out; try { out = p.parse(text); } catch (_) { return `<p>${esc(text)}</p>`; }
    const tags = count(out, '<', TAGS - used.tags);
    if (used.tags + tags > TAGS) { waited = false; used.over = true; return asText(text); }
    used.lines += lines; used.tags += tags;
    return out;
  }

  // ---------- streaming ----------
  // Where streamed text can be drawn for good: after a blank line or a closing fence, outside any
  // fence. Only lines not yet scanned are read, so a long reply costs its deltas, not its length
  // on each one. `st` is the caller's, one per stream.
  // `scan` is where the unfinished line starts, `seen` how far it was searched for its end, so a
  // long line arriving in pieces is searched once.
  function cut(st, text) {
    let i = st.scan ?? 0;
    for (;;) {
      const nl = text.indexOf('\n', Math.max(i, st.seen ?? 0)); if (nl < 0) break;
      const line = text.slice(i, nl), f = /^\s*(`{3,}|~{3,})(.*)$/.exec(line);
      if (st.fence) { if (f && f[1][0] === st.fence[0] && f[1].length >= st.fence.length && !f[2].trim()) { st.fence = null; st.cut = nl + 1; } }
      else if (f && !(f[1][0] === '`' && f[2].includes('`'))) st.fence = f[1];
      else if (!line.trim()) st.cut = nl + 1;
      i = nl + 1;
    }
    st.scan = i; st.seen = text.length;
    return st.cut ?? 0;
  }

  // ---------- drawn in place ----------
  // Something drawn later changes a block's height; a reader at the end of the pane stays there, and
  // one reading below the block keeps their place (the panes do no scroll anchoring of their own).
  function settle(box, change) {
    const pane = box.closest('.scroll'); if (!pane) { change(); return; }
    const end = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
    const above = !end && box.getBoundingClientRect().bottom <= pane.getBoundingClientRect().top, h = pane.scrollHeight;
    change();
    if (end) pane.scrollTop = pane.scrollHeight; else if (above) pane.scrollTop += pane.scrollHeight - h;
  }
  // Rendered diagrams by source (and a chart by its width), so a pane drawn again shows them at once;
  // at most 64 of them and 8 MiB of SVG.
  const diagrams = new Map(), DIAGRAMS = 64, DIAGRAM_BYTES = 8 * 1024 * 1024; let diagramBytes = 0;
  let mermaidChain = Promise.resolve(), diagramId = 0;
  function mermaidReady() {
    return script('vendor/mermaid.js').then(() => {
      if (!mermaidReady.done) {
        const css = getComputedStyle(document.documentElement), v = (n) => css.getPropertyValue(n).trim();
        globalThis.mermaid.initialize({
          startOnLoad: false, securityLevel: 'strict', suppressErrorRendering: true, theme: 'base', fontFamily: v('--mono'),
          themeVariables: { darkMode: true, fontFamily: v('--mono'), fontSize: '13px', background: v('--t-panel'), primaryColor: v('--t-raised'), primaryTextColor: v('--t-ink'), primaryBorderColor: v('--t-faint'), secondaryColor: v('--t-panel'), tertiaryColor: v('--t-bg'), lineColor: v('--t-dim'), textColor: v('--t-ink'), noteBkgColor: v('--t-raised'), noteTextColor: v('--t-ink'), noteBorderColor: v('--amber'), actorBkg: v('--t-raised'), actorBorder: v('--t-faint'), actorTextColor: v('--t-ink'), signalColor: v('--t-dim'), signalTextColor: v('--t-ink'), labelBoxBkgColor: v('--t-raised'), labelTextColor: v('--t-ink'), edgeLabelBackground: v('--t-panel'), clusterBkg: v('--t-panel'), clusterBorder: v('--t-line') },
        });
        mermaidReady.done = true;
      }
      return globalThis.mermaid;
    });
  }
  // A diagram or chart drawn to SVG once per source; one at a time, as Mermaid measures in a shared
  // scratch element. A source that does not draw keeps showing as code, the error in its head.
  // With `cached`, only one already drawn is shown: a chart drawn once, when asked, shows again.
  function drawLazy(box, src, make, cached = false) {
    const key = `${box.dataset.kind}|${box.dataset.lang ?? ''}|${box.dataset.kind === 'chart' ? box.clientWidth : ''}|${src}`;
    const show = (svg) => settle(box, () => { box.querySelector('.view').innerHTML = svg; box.dataset.view = 'view'; box.dataset.drawn = ''; });
    if (diagrams.has(key)) { show(diagrams.get(key)); return; }
    if (cached || 'asked' in box.dataset) return;
    box.dataset.asked = '';
    // One queued behind the same source takes its result instead of drawing it again; one larger
    // than the whole budget is shown but not kept.
    mermaidChain = mermaidChain.then(() => diagrams.get(key) ?? make(src, box)).then((svg) => {
      const cost = 2 * (key.length + svg.length);
      if (!diagrams.has(key) && cost <= DIAGRAM_BYTES) { diagrams.set(key, svg); diagramBytes += cost; }
      while (diagrams.size > DIAGRAMS || diagramBytes > DIAGRAM_BYTES) {
        const [k, v] = diagrams.entries().next().value; diagrams.delete(k); diagramBytes -= 2 * (k.length + v.length);
      }
      show(svg);
    }, (e) => { delete box.dataset.asked; const l = box.querySelector('.rh .lang'); l.textContent = `${l.textContent.split(' · ')[0]} · ${String(e?.message ?? e).split('\n')[0].slice(0, 80)}`; });
  }
  const mermaidSVG = (src) => mermaidReady().then((m) => m.render(`rich-mmd-${++diagramId}`, src)).then(({ svg }) => svg);

  // Charts: a Vega-Lite (or Vega) spec, drawn to static SVG in the window's colors. Data comes from
  // the spec alone: the loader refuses every URL, and expressions run in Vega's interpreter, never
  // as generated code. Categorical colors are the eight-hue order validated against the panel.
  const CATEGORY = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#008300', '#9085e9', '#e66767'];
  function chartTheme() {
    const css = getComputedStyle(document.documentElement), v = (n) => css.getPropertyValue(n).trim();
    const ink = v('--t-ink'), dim = v('--t-dim'), line = v('--t-line'), font = v('--mono');
    const axis = { domainColor: line, gridColor: line, gridOpacity: 0.6, tickColor: line, labelColor: dim, titleColor: dim, labelFont: font, titleFont: font, titleFontWeight: 500, labelFontSize: 11, titleFontSize: 11 };
    return {
      background: null, font, padding: 8,
      title: { color: ink, font, fontWeight: 600, fontSize: 13, anchor: 'start' },
      axis, legend: { labelColor: dim, titleColor: dim, labelFont: font, titleFont: font, titleFontWeight: 500 },
      view: { stroke: null },
      range: { category: CATEGORY, ordinal: { scheme: 'blues' }, ramp: ['#16263d', '#3987e5', '#bcd8f7'], diverging: ['#d95926', '#4a5062', '#3987e5'] },
      mark: { color: CATEGORY[0] }, bar: { cornerRadiusEnd: 4 }, line: { strokeWidth: 2 }, point: { size: 64, filled: true },
      text: { color: ink, font },
    };
  }
  async function chartSVG(src, box) {
    let spec; try { spec = JSON.parse(src); } catch (e) { throw new Error(`not JSON: ${e.message}`); }
    await script('vendor/vega.js');
    const { vega, vegaLite } = globalThis, theme = chartTheme();
    const lite = box.dataset.lang !== 'vega' && !/\/vega\/v\d/.test(spec.$schema ?? '');
    // A single view without a width fills the block.
    if (lite && spec.width == null && !['facet', 'concat', 'hconcat', 'vconcat', 'repeat'].some((k) => k in spec)) {
      spec = { ...spec, width: Math.max(200, Math.min(720, box.clientWidth - 140)), autosize: spec.autosize ?? { type: 'fit-x', contains: 'padding' } };
    }
    const runtime = vega.parse(lite ? vegaLite.compile(spec, { config: theme }).spec : spec, lite ? undefined : theme, { ast: true });
    const loader = vega.loader(); const refuse = () => Promise.reject(new Error('charts load no data from files or the network'));
    loader.load = refuse; loader.sanitize = refuse; loader.http = refuse; loader.file = refuse;
    const view = new vega.View(runtime, { renderer: 'none', loader, expr: vega.expressionInterpreter });
    try { return await view.toSVG(); } finally { view.finalize(); }
  }
  // The page a preview runs: nothing fetched, no frames, no forms; it reports its height so the
  // frame fits it, and its links go nowhere.
  const FRAME_HEAD = '<!doctype html><meta charset="utf-8">'
    + `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data: blob:; base-uri 'none'; form-action 'none'">`
    + '<script>(()=>{const post=()=>parent.postMessage({rich:"height",h:document.documentElement.scrollHeight},"*");addEventListener("load",post);new ResizeObserver(post).observe(document.documentElement);addEventListener("click",e=>{if(e.target.closest&&e.target.closest("a[href]"))e.preventDefault()},true)})()</script>';
  // A preview shown: a page in its frame, an SVG as an image (which runs no script and loads nothing).
  function mount(box) {
    const host = box.querySelector('.view'); if (!host || host.firstChild) return;
    const src = box.querySelector('pre').textContent;
    if (box.dataset.kind === 'svg') { host.innerHTML = `<img alt="" src="data:image/svg+xml;charset=utf-8,${esc(encodeURIComponent(src))}">`; return; }
    const f = document.createElement('iframe');
    f.setAttribute('sandbox', 'allow-scripts'); f.setAttribute('referrerpolicy', 'no-referrer'); f.title = 'HTML preview';
    f.srcdoc = FRAME_HEAD + src.replace(/^\s*<!doctype[^>]*>/i, '');
    host.append(f);
  }
  function unmount(box) { box.querySelector('.view')?.replaceChildren(); }
  function draw(box, cached = false) {
    const src = box.querySelector('pre').textContent;
    drawLazy(box, src, box.dataset.kind === 'mermaid' ? mermaidSVG : chartSVG, cached);
  }
  // After HTML from `html` is in the document: a file's drawing starts, and a diagram or chart already
  // drawn shows again from the cache. A page or SVG someone ran in a message is code again once its
  // pane is drawn anew: running it is asked of one block, once.
  function hydrate(root) {
    for (const box of root.querySelectorAll('.rc[data-lazy]:not([data-on]), .rc[data-kind="html"]:not([data-on]), .rc[data-kind="svg"]:not([data-on])')) {
      box.dataset.on = '';
      if ('lazy' in box.dataset) draw(box, !('run' in box.dataset));
      else if (box.dataset.view === 'view') mount(box);
    }
  }
  if (typeof window.addEventListener === 'function') window.addEventListener('message', (e) => {
    if (e.data?.rich !== 'height' || !(e.data.h > 0)) return;
    for (const f of document.querySelectorAll('.rc iframe')) if (f.contentWindow === e.source) { const h = `${Math.min(Math.ceil(e.data.h), Math.round(window.innerHeight * 0.8))}px`; if (f.style.height !== h) settle(f, () => { f.style.height = h; }); break; }
  });

  // ---------- clicks ----------
  function open(url) {
    const invoke = globalThis.__TAURI__?.core?.invoke;
    if (invoke) invoke('open_link', { url }).catch((e) => failed(String(e?.message ?? e))); else window.open(url, '_blank', 'noopener');
  }
  // Handles a click the transcript got; true when it was one of ours.
  function click(e) {
    const f = e.target.closest?.('[data-file]');
    if (f) { e.preventDefault(); openFile(f.dataset.file, f); return true; }
    const im = e.target.closest?.('button.img[data-img]');
    if (im) { const img = document.createElement('img'); img.src = im.dataset.img; img.alt = im.title; im.replaceWith(img); return true; }
    // Every link in drawn content goes through the guarded opener, or nowhere: a Mermaid `click`
    // link is an SVG `<a xlink:href>` that would otherwise take over the window.
    const a = e.target.closest?.('.md a, .rc a');
    if (a) { e.preventDefault(); const href = a.getAttribute('href') ?? a.getAttribute('xlink:href'); if (linkable(href)) open(href); return true; }
    const b = e.target.closest?.('[data-rich]'); if (!b) return false;
    const box = b.closest('.rc');
    if (b.dataset.rich === 'copy') {
      navigator.clipboard?.writeText(box.querySelector('pre').textContent).then(() => { b.textContent = 'copied'; setTimeout(() => { b.textContent = 'copy'; }, 1200); }, () => {});
    } else if (b.dataset.rich === 'view' && 'lazy' in box.dataset && !('drawn' in box.dataset)) {
      draw(box);
    } else if (b.dataset.rich === 'view' && (!('lazy' in box.dataset) || 'drawn' in box.dataset)) {
      box.dataset.view = box.dataset.view === 'view' ? 'code' : 'view';
      // A preview shown runs; one hidden stops, scripts, filters and all.
      if (box.dataset.kind === 'html' || box.dataset.kind === 'svg') { if (box.dataset.view === 'view') mount(box); else unmount(box); }
    }
    return true;
  }

  // ---------- files ----------
  // A file opened beside, drawn by its kind: Markdown, a diagram, a chart, a page, an image, a
  // table, or code. `bytes` is what was read (at most `cap`); `more` says the file goes on.
  const IMAGE = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', avif: 'image/avif', bmp: 'image/bmp', ico: 'image/x-icon' };
  const extOf = (path) => { const n = path.split('/').pop().toLowerCase(); return /\.(vl|vg)\.json$/.test(n) ? n.slice(-7, -5) : n.includes('.') ? n.split('.').pop() : n; };
  // The first `max` rows of a CSV or TSV, at most 256 columns each: a quoted field may hold the
  // separator, a line break or a doubled quote.
  const COLUMNS = 256, CELLS = 10000;
  function csv(text, sep, max) {
    const out = []; let row = [], cell = '', quoted = false, i = 0;
    const end = () => { if (row.length < COLUMNS) row.push(cell); cell = ''; if (row.length > 1 || row[0]) out.push(row); row = []; };
    for (; i < text.length && out.length < max; i++) {
      const c = text[i];
      if (quoted) { if (c !== '"') cell += c; else if (text[i + 1] === '"') { cell += c; i++; } else quoted = false; }
      else if (c === '"' && !cell) quoted = true;
      else if (c === sep) { if (row.length < COLUMNS) row.push(cell); cell = ''; }
      else if (c === '\n') end();
      else if (c !== '\r') cell += c;
    }
    if (out.length < max && (cell || row.length)) end();
    return out;
  }
  function table(text, sep) {
    const rows = csv(text, sep, 1001);
    const cell = (tag) => (c) => `<${tag}>${esc(c)}</${tag}>`;
    return `<div class="md"><table><thead><tr>${(rows[0] ?? []).map(cell('th')).join('')}</tr></thead><tbody>${rows.slice(1).map((r) => `<tr>${r.map(cell('td')).join('')}</tr>`).join('')}</tbody></table></div>`;
  }
  function file(path, bytes, more = false) {
    spent = 0;
    const ext = extOf(path), note = more ? `<div class="line note">showing the first ${Math.round(bytes.length / 1048576)} MiB</div>` : '';
    if (IMAGE[ext]) {
      if (more) return { html: '<div class="line note">image too large to show</div>' };
      const url = URL.createObjectURL(new Blob([bytes], { type: IMAGE[ext] }));
      return { html: `<div class="fimg"><img alt="" src="${esc(url)}"></div>`, url };
    }
    if (bytes.subarray(0, 8000).includes(0)) return { html: `<div class="line note">binary file · ${bytes.length}${more ? '+' : ''} bytes</div>` };
    const text = new TextDecoder().decode(bytes);
    if (ext === 'md' || ext === 'markdown') return { html: `${note}<div class="md">${html(text)}</div>` };
    if (ext === 'csv' || ext === 'tsv') return { html: note + table(text, ext === 'csv' ? ',' : '\t') };
    const lang = { mmd: 'mermaid', mermaid: 'mermaid', vl: 'vega-lite', vg: 'vega', htm: 'html', html: 'html', svg: 'svg' }[ext] ?? ext;
    return { html: note + `<div class="md">${block(text, lang, true)}</div>` };
  }
  let openFile = () => {}, failed = () => {};

  // A middle click on a link would open it in a new app window.
  document.addEventListener?.('auxclick', (e) => { if (e.target.closest?.('.md a, .rc a')) e.preventDefault(); });

  return { html, cut, hydrate, click, file, filePath, esc, get version() { return version; }, get waited() { return waited; }, set onReady(fn) { ready = fn; }, set onFile(fn) { openFile = fn; }, set onError(fn) { failed = fn; } };
})();
