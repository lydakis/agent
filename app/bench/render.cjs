// Message rendering cost in headless Chromium: 400 synthetic messages drawn by the renderer this
// app replaced and by rich.js, then a reply streamed in 8-character deltas. Medians of nine runs.
// The old messages are laid out under the stylesheet they shipped with, read from BASE; the new
// ones under the current app.css.
// Run from the repository: `node app/bench/render.cjs` with playwright-core resolvable
// (`NODE_PATH=<dir>/node_modules`) and CHROME set to a Chromium binary. Prints JSON.
const path = require('path');
const { execFileSync } = require('child_process');
const { chromium } = require('playwright-core');
const ui = path.join(__dirname, '..', 'ui');
const BASE = 'b7bdfe4';
(async () => {
  const b = await chromium.launch({ executablePath: process.env.CHROME });
  const p = await b.newPage({ viewport: { width: 1280, height: 780 } });
  await p.addStyleTag({ content: execFileSync('git', ['show', `${BASE}:app/ui/app.css`], { cwd: ui, encoding: 'utf8' }) });
  await p.addStyleTag({ path: path.join(ui, 'app.css') });
  for (const f of ['vendor/markdown-it.js', 'rich.js', 'vendor/highlight.js']) await p.addScriptTag({ path: path.join(ui, f) });
  const r = await p.evaluate(() => {
    const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
    // The renderer this replaces (`markdown` in BASE's app.js), verbatim.
    function inline(text) { return esc(text).replace(/\*\*(.+?)\*\*/g, '<h>$1</h>').replace(/`([^`]+)`/g, '<code>$1</code>'); }
    function old(text) {
      const out = []; let fence = null;
      for (const raw of text.split('\n')) {
        const m = raw.trimStart().match(/^```(.*)$/);
        if (m) { if (fence) { out.push(`<pre class="code">${fence.lang ? `<span class="lang">${esc(fence.lang)}</span>` : ''}${esc(fence.body.join('\n'))}</pre>`); fence = null; } else fence = { lang: m[1].trim(), body: [] }; continue; }
        if (fence) { fence.body.push(raw); continue; }
        const h = raw.trimStart().match(/^#+\s*(.*)$/);
        out.push(h ? `<div class="line text"><h>${inline(h[1])}</h></div>` : `<div class="line text">${inline(raw)}</div>`);
      }
      if (fence) out.push(`<pre class="code">${fence.lang ? `<span class="lang">${esc(fence.lang)}</span>` : ''}${esc(fence.body.join('\n'))}</pre>`);
      return out.join('');
    }
    // Each side is laid out under its own stylesheet: the first sheet is BASE's, the second the current one.
    const [oldCSS, newCSS] = document.styleSheets, sheet = (old) => { oldCSS.disabled = !old; newCSS.disabled = old; };
    const para = 'The store keeps **one writer** and a `query-only` reader, so a slow read never holds a commit. ';
    const msg = (i) => i % 3 ? `## Step ${i}\n\n${para.repeat(4)}\n\n- first point about \`rotate()\`\n- second point\n- third point\n\n${para.repeat(3)}` :
      `${para.repeat(2)}\n\n\`\`\`rust\n${'pub fn refresh(store: &Store, id: u64) -> Result<Token> {\n    let t = rotate(store, id)?; // pure\n    store.commit()?;\n    Ok(t)\n}\n'.repeat(4)}\`\`\`\n\n| a | b |\n|---|---|\n| 1 | 2 |`;
    const msgs = Array.from({ length: 400 }, (_, i) => msg(i));
    const bytes = msgs.reduce((a, m) => a + m.length, 0);
    const host = document.createElement('div'); host.className = 'scroll'; host.style.cssText = 'position:fixed;inset:0;z-index:99;background:#000;width:700px'; document.body.append(host);
    const time = (fn, n = 9) => { const ts = []; for (let k = 0; k < n; k++) { const t0 = performance.now(); fn(); ts.push(performance.now() - t0); } ts.sort((a, b) => a - b); return +ts[Math.floor(n / 2)].toFixed(1); };
    const draw = (html) => { host.innerHTML = html; void host.scrollHeight; };
    const out = { messages: msgs.length, kib: Math.round(bytes / 1024) };
    out.old_parse_ms = time(() => msgs.map(old).join(''));
    out.new_parse_ms = time(() => msgs.map((m) => `<div class="md">${Rich.html(m)}</div>`).join(''));
    const H = window.hljs; window.hljs = undefined; out.new_parse_nohl_ms = time(() => msgs.map((m) => Rich.html(m)).join('')); window.hljs = H;
    const codes = msgs.filter((m, i) => !(i % 3)).map((m) => m.split('```rust\n')[1].split('```')[0]);
    out.hl_only_ms = time(() => codes.map((c) => H.highlight(c, { language: 'rust', ignoreIllegals: true }).value));
    const plain = markdownit({ html: false, linkify: true, breaks: true }); out.markdown_it_plain_ms = time(() => msgs.map((m) => plain.render(m)));
    out.new_parse_again_ms = time(() => msgs.map((m) => `<div class="md">${Rich.html(m)}</div>`).join(''));
    const oldHTML = msgs.map(old).join(''), newHTML = msgs.map((m) => `<div class="md">${Rich.html(m)}</div>`).join('');
    sheet(true); out.old_dom_ms = time(() => draw(oldHTML)); sheet(false); out.new_dom_ms = time(() => draw(newHTML));
    out.new_dom_nohl_ms = time(() => draw(newHTML.replace(/<span class="hljs-[a-z_ .]*">|<\/span>/g, '')));
    out.new_dom_notable_ms = time(() => draw(newHTML.replace(/<table>[\s\S]*?<\/table>/g, '')));
    out.new_dom_noblock_ms = time(() => draw(newHTML.replace(/<div class="rc"[\s\S]*?<\/pre><\/div>/g, '')));
    out.old_html_kib = Math.round(oldHTML.length / 1024); out.new_html_kib = Math.round(newHTML.length / 1024);
    // Streaming: a 9 KiB reply in 8-character deltas, the old tail (append) against the new one (cut + draw
    // finished blocks into one message's bounds, hydrating what was added). Each delta does what the app's
    // render does around it: read whether the reader is at the bottom, then keep them there, so every delta
    // pays its own layout.
    const reply = msgs.slice(0, 12).join('\n\n'); const deltas = []; for (let i = 0; i < reply.length; i += 8) deltas.push(reply.slice(0, i + 8));
    out.stream_kib = Math.round(reply.length / 1024); out.stream_deltas = deltas.length;
    const follow = (fn) => { const atBottom = host.scrollHeight - host.scrollTop - host.clientHeight < 40; fn(); if (atBottom) host.scrollTop = host.scrollHeight; };
    sheet(true);
    out.stream_old_ms = time(() => { const tn = document.createTextNode(''); host.replaceChildren(tn); let off = 0; for (const v of deltas) follow(() => { tn.appendData(v.slice(off)); off = v.length; }); });
    sheet(false);
    out.stream_new_ms = time(() => { const done = document.createElement('div'), tn = document.createTextNode(''); done.className = 'md'; host.replaceChildren(done, tn); const st = {}, used = { lines: 0, tags: 0, code: 0, scope: 'bench|1' }; let drawn = 0, off = 0, blocks = 0;
      for (const v of deltas) follow(() => { const at = used.over ? 0 : Rich.cut(st, v); if (at > drawn) { const box = document.createElement('div'); box.innerHTML = Rich.html(v.slice(drawn, at), used); const added = [...box.childNodes]; done.append(...added); for (const n of added) if (n.nodeType === 1) Rich.hydrate(n); tn.data = v.slice(at); drawn = at; blocks++; } else tn.appendData(v.slice(off)); off = v.length; });
      out.stream_blocks = blocks; });
    out.stream_full_reparse_ms = time(() => { for (const v of deltas) Rich.html(v); }, 1);
    host.remove();
    return out;
  });
  console.log(JSON.stringify(r, null, 1));
  await b.close();
})();
