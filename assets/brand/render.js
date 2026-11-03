// Renders PNG icons and the GitHub social preview from the SVG/HTML sources in this directory.
// usage: node assets/brand/render.js   (needs playwright; fonts: Inter)
const { chromium } = require('playwright'); const fs = require('fs'); const path = require('path');
const D = __dirname; const rd = n => fs.readFileSync(path.join(D, n), 'utf8');
(async () => {
  const b = await chromium.launch(); const p = await b.newPage();
  for (const [name, size] of [['icon-512', 512], ['icon-192', 192], ['apple-touch-icon', 180], ['favicon-32', 32]]) {
    await p.setViewportSize({ width: size, height: size });
    await p.setContent(`<body style="margin:0;background:transparent">${rd('icon.svg').replace('<svg ', `<svg style="display:block" width="${size}" height="${size}" `)}`);
    await p.screenshot({ path: path.join(D, name + '.png'), omitBackground: true });
  }
  for (const [name, size] of [['logo-mark-512', 512]]) {
    await p.setViewportSize({ width: size, height: size });
    await p.setContent(`<body style="margin:0;background:transparent">${rd('logo-mark.svg').replace('<svg ', `<svg style="display:block" width="${size}" height="${size}" `)}`);
    await p.screenshot({ path: path.join(D, name + '.png'), omitBackground: true });
  }
  await p.setViewportSize({ width: 1280, height: 640 });
  await p.setContent(rd('social-preview.html').replace('__MARK__', rd('logo-mark.svg').replace(/<svg [^>]*>/, '<svg viewBox="0 0 256 256" width="400" height="400">')));
  await p.waitForTimeout(300);
  await p.screenshot({ path: path.join(D, 'social-preview.png') });
  await b.close();
})();
