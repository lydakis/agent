// Draws a Mermaid diagram to SVG with Pele, off the window's thread: a diagram that lays out slowly
// holds only this worker, which the window ends once it runs too long (see `diagramSVG` in rich.js).
// Links keep the schemes the window opens; images are off, as drawing a message fetches nothing. The
// SVG is held to a million characters (Pele's own bound is four): putting one that size into the page
// takes the window's thread about 150 ms, and no diagram a chat block can show needs more.
'use strict';
importScripts('vendor/pele.js');
onmessage = ({ data: { src, font, width, idPrefix } }) => {
  try {
    const { svg } = pele.render(src, { idPrefix, fontFamily: font, fontFamilyMono: font, fontSize: 13, maxWidth: width > 0 ? width : undefined, linkSchemes: ['http', 'https', 'mailto'], images: false, outputLimit: 1000000 });
    postMessage({ svg });
  } catch (e) {
    postMessage({ error: String(e?.message ?? e) });
  }
};
