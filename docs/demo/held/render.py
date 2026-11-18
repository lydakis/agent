import json, html, sys
NAMES = {'black':'#45475a','red':'#f38ba8','green':'#a6e3a1','brown':'#f9e2af','yellow':'#f9e2af','blue':'#89b4fa','magenta':'#f5c2e7','cyan':'#94e2d5','white':'#bac2de',
         'brightblack':'#585b70','brightred':'#f38ba8','brightgreen':'#a6e3a1','brightyellow':'#f9e2af','brightblue':'#89b4fa','brightmagenta':'#f5c2e7','brightcyan':'#94e2d5','brightwhite':'#a6adc8'}
FG, BG = '#e4e4e4', '#1f1f21'
XTERM = {'000000':'#3a3a3c','cd0000':'#e5534b','00cd00':'#57ab5a','cdcd00':'#e8b923','0000ee':'#4aa3f0','cd00cd':'#c678dd','00cdcd':'#39c5cf','e5e5e5':'#e4e4e4',
         '7f7f7f':'#8a8a8e','ff0000':'#f47067','00ff00':'#6bc46d','ffff00':'#f0c94a','5c5cff':'#6cb6ff','ff00ff':'#dcbdfb','00ffff':'#56d4dd','ffffff':'#ffffff'}
def color(c, default):
    if c == 'default': return default
    if c in NAMES: return NAMES[c]
    if c in XTERM: return XTERM[c]
    if len(c) == 6: return '#' + c
    return default
for name in sys.argv[1:]:
    rows = json.load(open(name + '.json'))
    last = max(i for i, r in enumerate(rows) if ''.join(c[0] for c in r).strip())
    rows = rows[:last + 2]
    out = []
    for r in rows:
        line = []
        for ch, fg, bg, bold, it, und, rev in r:
            f, b = color(fg, FG), color(bg, BG)
            if rev: f, b = b, f
            style = f'color:{f};background:{b}' + (';font-weight:bold' if bold else '') + (';font-style:italic' if it else '') + (';text-decoration:underline' if und else '')
            line.append(f'<span style="{style}">{html.escape(ch or " ")}</span>')
        out.append('<div class="r">' + ''.join(line) + '</div>')
    open(name + '.html', 'w').write(f'''<!doctype html><meta charset="utf-8"><style>
body{{margin:0;background:{BG}}} #t{{display:inline-block;padding:16px 20px;background:{BG};font:14px/1.35 "DejaVu Sans Mono",monospace;letter-spacing:0.2px;white-space:pre}}
.r{{height:19px}}</style><div id="t">{''.join(out)}</div>''')
