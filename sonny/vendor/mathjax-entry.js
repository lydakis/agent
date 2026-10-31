const {mathjax} = require('mathjax-full/js/mathjax.js');
const {TeX} = require('mathjax-full/js/input/tex.js');
const {SVG} = require('mathjax-full/js/output/svg.js');
const {liteAdaptor} = require('mathjax-full/js/adaptors/liteAdaptor.js');
const {RegisterHTMLHandler} = require('mathjax-full/js/handlers/html.js');
const {AllPackages} = require('mathjax-full/js/input/tex/AllPackages.js');
// One document for every formula, as MathJax's own server examples keep:
// the engine lives as long as Sonny, and a handler or TeX made per formula
// stays registered for good. Like formulas on one page, a macro one
// defines is known to the next; tags and labels start again each time,
// as MathJax.texReset() does.
const adaptor = liteAdaptor();
RegisterHTMLHandler(adaptor);
const tex = new TeX({packages: AllPackages});
const doc = mathjax.document('', {
  InputJax: tex,
  OutputJax: new SVG({fontCache: 'none'}),
});
globalThis.tex2svg = function (source, display) {
  tex.reset();
  const node = doc.convert(source, {display});
  doc.clear();
  return adaptor.innerHTML(node);
};
