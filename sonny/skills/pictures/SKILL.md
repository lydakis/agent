---
name: pictures
description: Show the person a diagram, chart or sketch inside Sonny by putting an SVG in a ```svg fence in your reply; Sonny draws it inline.
---

Sonny, the terminal the person reads you in, draws an SVG in your reply as a picture where the fence is. Use one when a picture says it better than text: how parts connect, a flow, a layout, a small chart.

````
```svg
<svg xmlns="http://www.w3.org/2000/svg" width="480" height="160" viewBox="0 0 480 160">
  <rect x="10" y="50" width="120" height="60" rx="8" fill="#e8f0fe" stroke="#1a73e8"/>
  <text x="70" y="85" font-size="16" text-anchor="middle" font-family="Helvetica">client</text>
  <line x1="130" y1="80" x2="340" y2="80" stroke="#555" stroke-width="2"/>
  <rect x="340" y="50" width="120" height="60" rx="8" fill="#e6f4ea" stroke="#188038"/>
  <text x="400" y="85" font-size="16" text-anchor="middle" font-family="Helvetica">daemon</text>
</svg>
```
````

- Give the root `<svg>` a `width`, `height` and `viewBox`. Around 480 to 800 wide reads well; it is scaled to fit the pane, no taller than about 30 lines.
- Fill the background (a full-size `<rect>`) or use colours that read on both light and dark: the terminal's own background shows through.
- Text at 14 or more, in Helvetica or another common font. No scripts, external images or fonts: they are not loaded.
- The SVG's source is not shown, so say in words what matters too.
- Where the terminal cannot draw pictures, the person sees the SVG source instead, so do not rely on one for something they must read.
