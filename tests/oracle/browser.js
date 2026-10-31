// Real-browser smoke of the unchanged frontend against a running web role.
// node tests/oracle/browser.js <playwright-core dir> <base-url> <out.json>
// Requires FONTCONFIG_FILE and CHROMIUM (path) in the environment.
// Synthetic corpus only (see tests/oracle/e2e.py).
'use strict';
const [, , playwrightDir, base, out] = process.argv;
const { chromium } = require(playwrightDir);
const fs = require('fs');

async function page(browser, width) {
  const context = await browser.newContext({ viewport: { width, height: 900 } });
  const tab = await context.newPage();
  const problems = [];
  tab.on('console', (m) => { if (['error', 'warning'].includes(m.type())) problems.push(`${m.type()}: ${m.text()}`); });
  tab.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
  tab.on('requestfailed', (r) => problems.push(`requestfailed: ${new URL(r.url()).pathname}`));
  tab.on('response', (r) => { if (r.status() >= 400) problems.push(`http ${r.status()}: ${new URL(r.url()).pathname}`); });
  return { context, tab, problems };
}

async function main() {
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM, args: ['--no-sandbox'] });
  const report = {};
  for (const width of [1280, 390]) {
    const { context, tab, problems } = await page(browser, width);
    await tab.goto(base + '/', { waitUntil: 'networkidle' });
    await tab.fill('#query', 'zypper update 방법');
    await tab.press('#query', 'Enter');
    await tab.waitForFunction(() => document.querySelectorAll('#results > *').length > 0, null, { timeout: 30000 });
    const results = await tab.evaluate(() => Array.from(document.querySelectorAll('#results > *'))
      .map((n) => n.textContent.replace(/\s+/g, ' ').trim().slice(0, 80)));
    const overflow = await tab.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
    const readLink = await tab.evaluate(() => {
      const a = Array.from(document.querySelectorAll('a')).find((x) => x.getAttribute('href') && x.getAttribute('href').startsWith('/read?'));
      return a ? a.getAttribute('href') : null;
    });
    let reader = null;
    if (readLink) {
      await tab.goto(base + readLink, { waitUntil: 'networkidle' });
      reader = await tab.evaluate(() => ({
        title: document.querySelector('h1.page-title') && document.querySelector('h1.page-title').textContent,
        copyButtons: document.querySelectorAll('button.copy-code').length,
        images: Array.from(document.querySelectorAll('article img')).map((i) => ({ src: i.getAttribute('src'), loaded: i.complete && i.naturalWidth > 0 })),
      }));
    }
    await tab.goto(base + '/graph', { waitUntil: 'networkidle' });
    await tab.waitForTimeout(4000);
    await tab.waitForFunction(() => /\d/.test(document.getElementById('node-count').textContent), null, { timeout: 30000 }).catch(() => {});
    const graph = await tab.evaluate(() => ({
      canvas: !!document.getElementById('map'),
      nodes: document.getElementById('node-count').textContent.trim(),
      edges: document.getElementById('edge-count').textContent.trim(),
      layout: document.getElementById('layout-status').textContent.trim(),
      message: document.getElementById('message').textContent.trim(),
    }));
    const status = await tab.evaluate(() => document.getElementById('status') && document.getElementById('status').textContent);
    report[width] = { results: results.length, firstResults: results.slice(0, 3), overflow, readLink: !!readLink, reader, graph, problems };
    await context.close();
  }
  await browser.close();
  fs.writeFileSync(out, JSON.stringify(report, null, 1));
  console.log(JSON.stringify(report));
}

main().catch((e) => { console.error(e); process.exit(1); });
