const puppeteer = require('puppeteer');
const path = require('path');
const fs = require('fs');

const htmlPath = process.argv[2];
const pngPath = process.argv[3];
const jsonPath = process.argv[4];
const viewportWidth = parseInt(process.argv[5] || '800', 10);

if (!htmlPath || !pngPath || !jsonPath) {
  console.error('Usage: node capture.js <html> <png> <json> <width>');
  process.exit(1);
}

const absPath = path.resolve(htmlPath);

(async () => {
  const browser = await puppeteer.launch({
    headless: 'new',
    args: ['--no-sandbox', '--disable-setuid-sandbox'],
  });
  const page = await browser.newPage();
  await page.setViewport({ width: viewportWidth, height: 600 });
  // 30s: multi-MB data-URI emails get close to the old 10s budget.
  await page.goto('file://' + absPath, { waitUntil: 'networkidle0', timeout: 30000 });
  // networkidle0 does not cover data-URI @font-face decoding; without
  // this the measurement pass can run against fallback-font geometry.
  await page.evaluate(() => document.fonts.ready.then(() => true));

  const elements = await page.evaluate(() => {
    const results = [];
    function walk(node, parentPath) {
      if (node.nodeType !== Node.ELEMENT_NODE) return;
      const cs = window.getComputedStyle(node);
      const tag = node.tagName.toLowerCase();
      if (cs.display === 'none') return;

      let sibIdx = 0;
      let sib = node.previousElementSibling;
      while (sib) {
        if (sib.tagName === node.tagName) sibIdx++;
        sib = sib.previousElementSibling;
      }
      const nodePath = parentPath ? `${parentPath}>${tag}[${sibIdx}]` : tag;
      const rect = node.getBoundingClientRect();

      results.push({
        path: nodePath,
        tag,
        x: Math.round(rect.left * 10) / 10,
        y: Math.round(rect.top * 10) / 10,
        w: Math.round(rect.width * 10) / 10,
        h: Math.round(rect.height * 10) / 10,
      });

      for (const child of node.children) {
        walk(child, nodePath);
      }
    }
    walk(document.documentElement, '');
    return results;
  });

  fs.mkdirSync(path.dirname(jsonPath), { recursive: true });
  fs.writeFileSync(jsonPath, JSON.stringify(elements));
  console.error(`Extracted ${elements.length} elements`);

  // Sidecar so a silent Chrome update changing fractional line heights
  // is attributable instead of looking like mysterious drift across
  // every fixture at once. Separate file: neither JSON parser changes.
  const meta = {
    browser: await browser.version(),
    puppeteer: require('puppeteer/package.json').version,
    viewport: page.viewport(),
    timestamp: new Date().toISOString(),
  };
  const metaPath = path.join(path.dirname(jsonPath), 'chrome.meta.json');
  fs.writeFileSync(metaPath, JSON.stringify(meta, null, 2) + '\n');

  // fullPage captures the whole scrollable document; the viewport stays
  // at its measurement-pass size so viewport-dependent layout cannot
  // shift between the JSON dump and the screenshot.
  const bodyHeight = await page.evaluate(() => document.body.scrollHeight);
  fs.mkdirSync(path.dirname(pngPath), { recursive: true });
  await page.screenshot({ path: pngPath, fullPage: true });
  console.error(`Screenshot: ${bodyHeight}px`);

  await browser.close();
})();
