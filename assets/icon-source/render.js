const { chromium } = require('playwright');
(async () => {
  const b = await chromium.launch();
  const p = await b.newPage({ viewport: { width: 1000, height: 1000 }, deviceScaleFactor: 1 });
  for (const [svg, png] of process.argv.slice(2).reduce((a, v, i, arr) => (i % 2 ? a : [...a, [v, arr[i + 1]]]), [])) {
    await p.goto('file://' + svg);
    await p.screenshot({ path: png, omitBackground: true });
  }
  await b.close();
})();
