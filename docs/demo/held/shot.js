const { chromium } = require('playwright');
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ deviceScaleFactor: 2 });
  for (const n of process.argv.slice(2)) {
    await p.goto('file://' + process.cwd() + '/' + n + '.html');
    await (await p.$('#t')).screenshot({ path: n + '.png' });
  }
  await b.close();
})();
