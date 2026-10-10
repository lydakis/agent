// What a model writes, drawn as a page draws it: Markdown, highlighted code, Mermaid diagrams,
// Vega-Lite charts and HTML previews, in a message or a file opened beside. Markdown is parsed once
// per message (markdown-it, loaded with the page); highlighting, Mermaid and Vega load the first time
// something needs them. Raw HTML in Markdown stays text; an ```html block runs only when asked, in a
// sandboxed frame with no network and no way into the app; a chart loads no data from anywhere.
window.Rich = (() => {
  'use strict';
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const linkable = (href) => /^(https?:|mailto:)/i.test(href ?? '');
  // Bumped when highlighting arrives, so HTML drawn without it is drawn again (see `ready`).
  // `waited` says the last `html` drew code plain while highlighting loads, so only such a
  // message needs drawing again once it arrives.
  let version = 0, ready = () => {}, escaped = () => {}, waited = false;

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
  // A message or file highlights at most 256 KiB of code in all (`spent`, set by `html` and
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
  // A diagram or chart in a message carries an id: where it was drawn (`scope`, its message or
  // file), which of the diagrams and charts there it is (`nth`), and what it draws. The message
  // drawn anew, for highlighting or a pane redrawn, gives it the same id, so the one someone asked
  // for stays shown; every other block, a copy of it included, still asks. Without a scope each
  // block is its own. While a reply streams (`draft`), its diagrams, charts and pages are code: one
  // is drawn once the reply is in and has the id it keeps.
  let scope = null, nth = 0, blockId = 0, draft = false;
  const digest = (s) => { let a = 0x811c9dc5, b = 5381; for (let i = 0; i < s.length; i++) { const c = s.charCodeAt(i); a = Math.imul(a ^ c, 0x01000193); b = (Math.imul(b, 33) + c) | 0; } return `${s.length}.${(a >>> 0).toString(36)}.${(b >>> 0).toString(36)}`; };
  const lazy = (page, kind, text) => page ? ' data-page' : ` data-id="${esc(scope == null ? `#${++blockId}` : `${scope}|${nth++}|${kind}|${digest(text)}`)}"`;
  // Whether text is an SVG document: its root is `<svg>`, after an XML declaration, comments and a
  // doctype if it has them. Read in one pass, as a generated file can open with many comments.
  function isSVG(text) {
    let i = 0;
    const skip = () => { while (i < text.length && /\s/.test(text[i])) i++; };
    skip();
    if (text.startsWith('<?xml', i)) { const j = text.indexOf('?>', i); if (j < 0) return false; i = j + 2; }
    for (;;) {
      skip();
      if (text.startsWith('<!--', i)) { const j = text.indexOf('-->', i + 4); if (j < 0) return false; i = j + 3; }
      else if (text.slice(i, i + 9).toLowerCase() === '<!doctype') {
        const b = text.indexOf('[', i), g = text.indexOf('>', i); if (g < 0) return false;
        if (b >= 0 && b < g) { const e = text.indexOf(']', b); if (e < 0) return false; const k = text.indexOf('>', e); if (k < 0) return false; i = k + 1; } else i = g + 1;
      } else return /^<svg[\s>/]/i.test(text.slice(i, i + 5));
    }
  }
  function block(text, info, page = false) {
    const lang = (info ?? '').trim().split(/\s+/)[0].toLowerCase();
    if (draft) return `<div class="rc" data-kind="code">${head(lang, '')}${pre(text, lang)}</div>`;
    if (lang === 'mermaid' || lang === 'mmd') return `<div class="rc" data-kind="mermaid" data-lazy${lazy(page, 'mermaid', text)} data-view="code">${head('mermaid', toggle)}<div class="view"></div>${pre(text, '')}</div>`;
    if (CHART.has(lang)) return `<div class="rc" data-kind="chart" data-lang="${lang === 'vega' ? 'vega' : 'vega-lite'}" data-lazy${lazy(page, lang, text)} data-view="code">${head(lang, toggle)}<div class="view"></div>${pre(text, 'json')}</div>`;
    if (lang === 'html' || lang === 'htm') return `<div class="rc" data-kind="html" data-view="${page ? 'view' : 'code'}">${head(lang, '<button type="button" data-rich="view"></button>')}<div class="view frame"></div>${pre(text, 'xml')}</div>`;
    // An SVG draws as an image, which runs no script and loads nothing.
    if (lang === 'svg' && isSVG(text)) return `<div class="rc" data-kind="svg" data-view="${page ? 'view' : 'code'}">${head(lang, '<button type="button" data-rich="view"></button>')}<div class="view"></div>${pre(text, 'xml')}</div>`;
    return `<div class="rc" data-kind="code">${head(lang, '')}${pre(text, lang)}</div>`;
  }
  // A reference defined once can be used thousands of times, and each use copies its target into
  // the page: a message's links and images carry at most 1 Mi characters of targets and titles in
  // all (`linkLeft`, shared like the other bounds); past that a link is its text. Each is charged
  // as written into the page, escaped, and an image's target and text as often as it writes them.
  const LINK_CHARS = 1 << 20;
  let linkLeft = LINK_CHARS;
  // Read no further than the budget left, so a target used past it costs its length, not a scan.
  const linkCost = (href, title, k = 1) => {
    href ??= ''; title ??= ''; let n = k * (href.length + title.length);
    for (const s of [href, title]) for (let i = 0; i < s.length && n <= linkLeft; i++) { const c = s.charCodeAt(i); n += k * (c === 34 ? 5 : c === 38 || c === 39 ? 4 : c === 60 || c === 62 ? 3 : 0); }
    if (n > linkLeft) return false; linkLeft -= n; return true;
  };
  // Markdown is parsed by markdown-it, whose work grows with its input: a hostile reply can make
  // a parser's time grow faster than its length, and this one runs on the window's thread.
  let md = null; const closes = [];
  function parser() {
    if (md || !globalThis.markdownit) return md;
    md = globalThis.markdownit({ html: false, linkify: true, breaks: true });
    // Which links open, and how, is decided below; a link the window would not open is its text.
    md.validateLink = () => true;
    const r = md.renderer.rules;
    r.fence = (tokens, i) => block(tokens[i].content.replace(/\n$/, ''), tokens[i].info);
    r.code_block = (tokens, i) => block(tokens[i].content.replace(/\n$/, ''), '');
    r.rich_source = (tokens, i) => block(tokens[i].content, '');
    // Every link drawn is inert: its target is data (`data-href` for a URL, `data-file` for a path,
    // which opens beside from the agent's folder) and only the click handler acts on it, so no
    // native path (a context menu, a middle click, a drag) can follow it in the window. The `#`
    // href lets Tab and Enter reach it.
    r.link_open = (tokens, i) => {
      const href = tokens[i].attrGet('href') ?? '', title = tokens[i].attrGet('title'), t = title ? ` title="${esc(title)}"` : '';
      const open = !linkCost(href, title) ? '' : linkable(href) ? `<a href="#" data-href="${esc(href)}"${t}>` : filePath(href) ? `<a class="file" href="#" data-file="${esc(filePath(href))}"${t}>` : '';
      closes.push(open ? '</a>' : ''); return open;
    };
    r.link_close = () => closes.pop() ?? '';
    // An image draws only from data the message carries, and only on a click: a small PNG can
    // decode to hundreds of megabytes and an animated one takes CPU for as long as it shows.
    // A remote image is a link and a local one opens beside, so drawing a message fetches
    // nothing a model chose.
    r.image = (tokens, i, options, env, self) => {
      const href = tokens[i].attrGet('src') ?? '', text = self.renderInlineAsText(tokens[i].children ?? [], options, env);
      return !linkCost(href, text, 2) ? esc(text) : /^data:image\/(png|gif|jpe?g|webp)[;,]/i.test(href) ? `<button type="button" class="img" data-img="${esc(href)}" title="${esc(text)}">image${text ? `: ${esc(text)}` : ''}</button>` : linkable(href) ? `<a href="#" data-href="${esc(href)}">${esc(text || href)}</a>` : filePath(href) ? `<a class="file" href="#" data-file="${esc(filePath(href))}">${esc(text || href)}</a>` : esc(text);
    };
    // A table past 256 columns or 10,000 cells shows as its source, before its cells are parsed: a
    // short row is padded to the header's width, so a few bytes a row can ask for millions of cells.
    md.core.ruler.after('block', 'rich_table', (state) => {
      const out = []; let starts = null;
      for (let i = 0; i < state.tokens.length; i++) {
        const t = state.tokens[i];
        if (t.type !== 'table_open') { out.push(t); continue; }
        let end = i, cols = 0, rows = 0, head = true;
        for (; state.tokens[end].type !== 'table_close'; end++) {
          const k = state.tokens[end].type;
          if (k === 'th_open' && head) cols++; else if (k === 'tr_open') rows++; else if (k === 'thead_close') head = false;
        }
        if (cols <= COLUMNS && cols * rows <= CELLS) { out.push(...state.tokens.slice(i, end + 1)); i = end; continue; }
        if (!starts) { starts = [0]; for (let j = state.src.indexOf('\n'); j !== -1; j = state.src.indexOf('\n', j + 1)) starts.push(j + 1); }
        const src = new state.Token('rich_source', '', 0);
        src.content = state.src.slice(starts[t.map[0]], t.map[1] < starts.length ? starts[t.map[1]] : state.src.length).replace(/\n+$/, '');
        out.push(src); i = end;
      }
      state.tokens = out;
    });
    // `- [ ] item` and `- [x] item` are task boxes, which the window shows but nobody ticks.
    md.core.ruler.after('inline', 'rich_tasks', (state) => {
      const ts = state.tokens;
      for (let i = 2; i < ts.length; i++) {
        const first = ts[i].type === 'inline' && ts[i - 1].type === 'paragraph_open' && ts[i - 2].type === 'list_item_open' ? ts[i].children?.[0] : null;
        const m = first?.type === 'text' && /^\[([ xX])\] /.exec(first.content);
        if (!m) continue;
        first.content = first.content.slice(4);
        const box = new state.Token('html_inline', '', 0); box.content = `<input type="checkbox" disabled${m[1] === ' ' ? '' : ' checked'}> `;
        ts[i].children.unshift(box);
      }
    });
    return md;
  }
  // A link's target as a path, without its fragment (`a.md#install`, `a.rs#L12`) or a line suffix
  // (`a.rs:12`); null for a URL or anchor. A `#` or `:` in a file's name is written `%23` or `%3A`.
  function filePath(href) {
    const scheme = /^[a-z][a-z0-9+.-]*:/i;
    let p = (href ?? '').replace(/#.*$/s, '').replace(/^file:\/\//i, '');
    const bare = p.replace(/:\d+(:\d+)?$/, '');
    // `a.rs:12` is a file and its line; `tel:12345` keeps its scheme, as what precedes the number
    // names no file.
    if (!bare || scheme.test(bare) || (bare !== p && !/[./]/.test(bare))) return null;
    p = bare; try { p = decodeURIComponent(p); } catch (_) {}
    return p || null;
  }
  // A message's HTML, inside the caller's `.md` box. One that would draw past 100,000 tags (about
  // 50,000 elements) shows as its text: a line of `- x` or a `*x*` makes an element from a few
  // bytes, and the window pays for every element it holds. It is not parsed past 50,000 lines or
  // 100,000 marks that open an inline element (`*`, `_`, a backtick, `[`, `!`, `<`, `~`, `|`, `@`, `\`, `&`,
  // `www.`, `://`), since the parser's tokens cost more than the HTML they become.
  const TAGS = 100000, LINES = 50000;
  const count = (s, c, max) => { let n = 0, i = -1; while (n <= max && (i = s.indexOf(c, i + 1)) !== -1) n++; return n; };
  const MARK = /[*_`[<~|@!\\&]|www\.|:\/\//g;
  const marks = (s, max) => { let n = 0; MARK.lastIndex = 0; while (n <= max && MARK.exec(s)) n++; return n; };
  const asText = (text) => `<div class="rc" data-kind="code">${head('text', '')}<pre class="code"><code>${esc(text)}</code></pre></div>`;
  // `used` carries the bounds across the pieces of one message drawn apart, as a streamed reply's
  // blocks are; once over, `used.over` is set and that piece is text. `used.scope` names where its
  // diagrams and charts were drawn and `used.blocks` counts those drawn before it (see `lazy`);
  // `used.marks` counts the marks parsed, which is what the parsing cost;
  // `used.draft` marks a reply still streaming.
  function html(text, used = { lines: 0, tags: 0, code: 0 }) {
    waited = false; spent = used.code ?? 0; scope = used.scope ?? null; nth = used.blocks ?? 0; draft = !!used.draft; linkLeft = LINK_CHARS - (used.links ?? 0);
    const p = parser();
    if (!p) return `<p>${esc(text)}</p>`;
    if (used.over) return asText(text);
    const lines = count(text, '\n', LINES - used.lines);
    const mk = marks(text, TAGS - Math.max(used.tags, used.marks ?? 0));
    if (used.lines + lines > LINES || used.tags + mk > TAGS || (used.marks ?? 0) + mk > TAGS) { used.over = true; return asText(text); }
    closes.length = 0;
    let out; try { out = p.render(text); } catch (_) { return `<p>${esc(text)}</p>`; }
    used.code = spent;
    const tags = count(out, '<', TAGS - used.tags);
    if (used.tags + tags > TAGS) { waited = false; used.over = true; return asText(text); }
    used.lines += lines; used.tags += tags; used.marks = (used.marks ?? 0) + mk; used.blocks = nth; used.links = LINK_CHARS - linkLeft;
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
  // `hold` notes the reader's place before a change; each call of what it returns keeps it after.
  // While a pane is drawn whole its caller keeps the place once, after (`hydrate(root, false)`).
  let holding = true;
  function hold(box) {
    const pane = holding && box.closest?.('.scroll'); if (!pane) return () => {};
    const end = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
    const above = !end && box.getBoundingClientRect().bottom <= pane.getBoundingClientRect().top; let h = pane.scrollHeight;
    return () => { if (end) pane.scrollTop = pane.scrollHeight; else if (above) pane.scrollTop += pane.scrollHeight - h; h = pane.scrollHeight; };
  }
  function settle(box, change) { const keep = hold(box); change(); keep(); }
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
  // scratch element. A source that does not draw keeps showing as code, the error in its head,
  // and draws again only when asked again.
  // With `cached`, only a block someone asked for (`shown`, the last 1,024 asked) draws again,
  // from the cache or, when a chart's width changed, anew.
  const shown = new Set(), SHOWN = 1024;
  function drawLazy(box, src, make, cached = false) {
    const key = `${box.dataset.kind}|${box.dataset.lang ?? ''}|${box.dataset.kind === 'chart' ? box.clientWidth : ''}|${src}`;
    const show = (svg) => settle(box, () => { box.querySelector('.view').innerHTML = svg; box.dataset.view = 'view'; box.dataset.drawn = ''; });
    if (cached && !shown.has(box.dataset.id)) return;
    if (!cached && box.dataset.id) { shown.delete(box.dataset.id); shown.add(box.dataset.id); if (shown.size > SHOWN) shown.delete(shown.values().next().value); }
    if (diagrams.has(key)) { show(diagrams.get(key)); return; }
    if ('asked' in box.dataset) return;
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
    }, (e) => { delete box.dataset.asked; shown.delete(box.dataset.id); const l = box.querySelector('.rh .lang'); l.textContent = `${l.textContent.split(' · ')[0]} · ${String(e?.message ?? e).split('\n')[0].slice(0, 80)}`; });
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
  // frame fits it, its links go nowhere, and Escape pressed in it is the window's (a frame's keys
  // do not reach its parent).
  const FRAME_HEAD = '<!doctype html><meta charset="utf-8">'
    + `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; media-src data: blob:; base-uri 'none'; form-action 'none'">`
    + '<script>(()=>{const post=()=>parent.postMessage({rich:"height",h:document.documentElement.scrollHeight},"*");addEventListener("load",post);new ResizeObserver(post).observe(document.documentElement);addEventListener("click",e=>{if(e.target.closest&&e.target.closest("a[href]"))e.preventDefault()},true);addEventListener("keydown",e=>{if(e.key==="Escape")parent.postMessage({rich:"escape"},"*")})})()</script>';
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
  // After HTML from `html` is in the document (`root` and what it holds): a file's drawing starts,
  // and a diagram or chart someone asked for shows again from the cache. A page or SVG someone ran
  // in a message is code again once its pane is drawn anew: running it is asked of one block, once.
  const HYDRATE = '.rc[data-lazy]:not([data-on]), .rc[data-kind="html"]:not([data-on]), .rc[data-kind="svg"]:not([data-on])';
  function hydrate(root, anchor = true) {
    holding = anchor;
    try {
      for (const box of [...(root.matches?.(HYDRATE) ? [root] : []), ...root.querySelectorAll(HYDRATE)]) {
        box.dataset.on = '';
        if ('lazy' in box.dataset) draw(box, !('page' in box.dataset));
        else if (box.dataset.view === 'view') mount(box);
      }
    } finally { holding = true; }
  }
  // Another store's blocks are not the ones asked for here, whatever their ids.
  const forget = () => shown.clear();
  if (typeof window.addEventListener === 'function') window.addEventListener('message', (e) => {
    // Only a frame the reader is in can hand the window its Escape.
    if (e.data?.rich === 'escape') { for (const f of document.querySelectorAll('.rc iframe')) if (f.contentWindow === e.source && document.activeElement === f) { escaped(f); break; } return; }
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
    // It has no height until decoded, so the reader's place is kept again once it loads.
    if (im) { const img = document.createElement('img'), keep = hold(im); img.addEventListener('load', keep); img.src = im.dataset.img; img.alt = im.title; im.replaceWith(img); keep(); return true; }
    // Every link in drawn content goes through the guarded opener, or nowhere: a Mermaid `click`
    // link is an SVG `<a xlink:href>` that would otherwise take over the window.
    const a = e.target.closest?.('.md a, .rc a');
    if (a) { e.preventDefault(); const href = a.dataset?.href ?? a.getAttribute('href') ?? a.getAttribute('xlink:href'); if (linkable(href)) open(href); return true; }
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
  // `asked` draws a page, diagram, chart or image at once; without it (a file an agent rewrote
  // while open) each waits for a click, as in a message.
  const IMAGE = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', avif: 'image/avif', bmp: 'image/bmp', ico: 'image/x-icon' };
  const extOf = (path) => { const n = path.split('/').pop().toLowerCase(); return /\.(vl|vg)\.json$/.test(n) ? n.slice(-7, -5) : n.includes('.') ? n.split('.').pop() : n; };
  // The first `max` rows of a CSV or TSV, at most 256 columns each, ending with the row that
  // reaches 10,000 cells: a quoted field may hold the separator, a line break or a doubled quote.
  // `more` says rows were left out.
  const COLUMNS = 256, CELLS = 10000;
  function csv(text, sep, max) {
    const out = []; let row = [], cell = '', quoted = false, i = 0, cells = 0;
    const end = () => { if (row.length < COLUMNS) row.push(cell); cell = ''; if (row.length > 1 || row[0]) { out.push(row); cells += row.length; } row = []; };
    for (; i < text.length && out.length < max && cells < CELLS; i++) {
      const c = text[i];
      if (quoted) { if (c !== '"') cell += c; else if (text[i + 1] === '"') { cell += c; i++; } else quoted = false; }
      else if (c === '"' && !cell) quoted = true;
      else if (c === sep) { if (row.length < COLUMNS) row.push(cell); cell = ''; }
      else if (c === '\n') end();
      else if (c !== '\r') cell += c;
    }
    if (out.length < max && cells < CELLS && (cell || row.length)) end();
    out.more = /\S/.test(text.slice(i));
    return out;
  }
  function table(text, sep) {
    const rows = csv(text, sep, 1001);
    const cell = (tag) => (c) => `<${tag}>${esc(c)}</${tag}>`;
    return `${rows.more ? `<div class="line note">showing the first ${rows.length - 1} rows</div>` : ''}<div class="md"><table><thead><tr>${(rows[0] ?? []).map(cell('th')).join('')}</tr></thead><tbody>${rows.slice(1).map((r) => `<tr>${r.map(cell('td')).join('')}</tr>`).join('')}</tbody></table></div>`;
  }
  // `waited` says the view is code that highlighting, once loaded, would draw differently.
  function file(path, bytes, more = false, asked = true) {
    spent = 0; waited = false; scope = `file ${path}`; nth = 0; draft = false;
    const ext = extOf(path), note = more ? `<div class="line note">showing the first ${Math.round(bytes.length / 1048576)} MiB</div>` : '';
    if (IMAGE[ext]) {
      if (more) return { html: '<div class="line note">image too large to show</div>' };
      const url = URL.createObjectURL(new Blob([bytes], { type: IMAGE[ext] })), name = path.split('/').pop();
      return { html: asked ? `<div class="fimg"><img alt="" src="${esc(url)}"></div>` : `<div class="md fimg"><button type="button" class="img" data-img="${esc(url)}" title="${esc(name)}">image: ${esc(name)}</button></div>`, url };
    }
    if (bytes.subarray(0, 8000).includes(0)) return { html: `<div class="line note">binary file · ${bytes.length}${more ? '+' : ''} bytes</div>` };
    const text = new TextDecoder().decode(bytes);
    if (ext === 'md' || ext === 'markdown') return { html: `${note}<div class="md">${html(text, { lines: 0, tags: 0, code: 0, scope })}</div>`, waited };
    if (ext === 'csv' || ext === 'tsv') return { html: note + table(text, ext === 'csv' ? ',' : '\t') };
    const lang = { mmd: 'mermaid', mermaid: 'mermaid', vl: 'vega-lite', vg: 'vega', htm: 'html', html: 'html', svg: 'svg' }[ext] ?? ext;
    const out = block(text, lang, asked);
    return { html: note + `<div class="md">${out}</div>`, waited: waited && out.startsWith('<div class="rc" data-kind="code"') };
  }
  let openFile = () => {}, failed = () => {};

  // A middle click on a link would open it in a new app window.
  document.addEventListener?.('auxclick', (e) => { if (e.target.closest?.('.md a, .rc a, a[data-file]')) e.preventDefault(); });

  return { html, cut, hydrate, forget, click, file, filePath, esc, get version() { return version; }, get waited() { return waited; }, set onReady(fn) { ready = fn; }, set onEscape(fn) { escaped = fn; }, set onFile(fn) { openFile = fn; }, set onError(fn) { failed = fn; } };
})();
