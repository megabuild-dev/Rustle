// Browser end-to-end check of the embedded dashboard against a dev-fakes server.
// usage: RUSTLE_BIN=target/release/rustle node scripts/ui_e2e.js [screenshot-dir]
const { chromium } = require('playwright');
const { spawn, execFileSync } = require('child_process');
const fs = require('fs'), os = require('os'), path = require('path');

const BIN = process.env.RUSTLE_BIN || 'target/release/rustle';
const PORT = process.env.RUSTLE_SMOKE_PORT || '39002';
const BASE = `http://127.0.0.1:${PORT}`;
const SHOTS = process.argv[2];

function assert(c, m) { if (!c) { throw new Error('ASSERT: ' + m); } }

(async () => {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), 'rustle-e2e-'));
  const env = { ...process.env, RUSTLE_STATE_DIR: d, RUSTLE_MASTER_KEY_FILE: d + '/master.key', RUSTLE_DB: d + '/rustle.db',
    RUSTLE_BIND: '127.0.0.1:' + PORT, RUSTLE_SOCKET: 'none', RUSTLE_FAKE_BACKENDS: '1' };
  const out = execFileSync(BIN, ['init'], { env }).toString();
  const password = /password: (\S+)/.exec(out)[1];
  const srv = spawn(BIN, [], { env, stdio: 'ignore' });
  const errors = [];
  let browser;
  try {
    for (let i = 0; i < 50; i++) { try { const r = await fetch(BASE + '/healthz'); if (r.ok) break; } catch {} await new Promise(r => setTimeout(r, 100)); }
    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    page.on('console', m => { if (m.type() === 'error' && !/status of 401/.test(m.text())) errors.push('console: ' + m.text()); });
    page.on('pageerror', e => errors.push('pageerror: ' + e.message));
    const shot = async n => { if (SHOTS) { fs.mkdirSync(SHOTS, { recursive: true }); await page.screenshot({ path: path.join(SHOTS, n + '.png') }); } };

    await page.goto(BASE + '/');
    await page.waitForSelector('.login form', { timeout: 15000 });
    await shot('1-login');
    // wrong password is rejected with a visible message
    await page.fill('input[type=password]', 'wrong-password');
    await page.click('button.primary');
    await page.waitForSelector('.toast.bad', { timeout: 5000 });
    // correct login
    await page.fill('input[type=password]', password);
    await page.click('button.primary');
    await page.waitForSelector('aside nav', { timeout: 10000 });
    await page.waitForSelector('.cards .card', { timeout: 10000 });
    assert((await page.textContent('.strip')).includes('Rustle process'), 'overview shows the process RSS');
    await shot('2-overview');

    // add a site through the wizard
    await page.click('header button.primary');
    await page.waitForSelector('.modal');
    const inputs = page.locator('.modal .form input');
    await inputs.nth(0).fill('blog');
    await inputs.nth(1).fill('blog.example.com');
    await inputs.nth(2).fill('ghcr.io/acme/blog');
    await shot('3-wizard-1');
    await page.click('.modal-foot button.primary');           // -> step 2
    await page.click('.modal-foot button.primary');           // -> step 3
    await page.locator('.modal .form label.check input').first().check(); // skip DNS (fakes have no DNS)
    await page.locator('.modal .form label.check input').nth(1).check(); // allow budget override
    await page.click('.modal-foot button.primary');           // create
    await page.waitForSelector('.modal .banner.good', { timeout: 10000 });
    const token = (await page.locator('.modal .code pre').first().textContent()).trim();
    assert(token.startsWith('rt_'), 'wizard shows the deploy token once');
    await shot('4-wizard-done');
    await page.click('.modal-foot button.primary');           // Done

    // deploy through the webhook like GitHub Actions would
    const r = await fetch(BASE + '/api/v1/webhooks/deploy', { method: 'POST', headers: { authorization: 'Bearer ' + token, 'content-type': 'application/json' }, body: JSON.stringify({ image: 'ghcr.io/acme/blog:abc123' }) });
    assert(r.status === 202, 'webhook accepted: ' + r.status);

    await page.goto(BASE + '/#/sites');
    await page.waitForSelector('.list .row');
    await page.waitForFunction(() => document.querySelector('.list .row .badge')?.textContent === 'running', null, { timeout: 15000 });
    await shot('5-sites');
    await page.click('.list .row');
    await page.waitForSelector('.tabs');
    // deploys tab shows the webhook deployment
    await page.click('.tabs a:has-text("Deploys")');
    await page.waitForSelector('table tbody tr', { timeout: 10000 });
    assert((await page.textContent('table tbody')).includes('webhook'), 'deploy history lists the webhook trigger');
    await shot('6-deploys');
    // environment: add a variable, it is stored masked
    await page.click('.tabs a:has-text("Environment")');
    await page.click('button:has-text("+ Add variable")');
    await page.locator('.env-row input').nth(0).fill('API_KEY');
    await page.locator('.env-row input').nth(1).fill('s3cret-value');
    await page.click('.panel button.primary');
    await page.waitForSelector('.toast.good');
    await page.reload();
    await page.click('.tabs a:has-text("Environment")').catch(() => {});
    await page.waitForSelector('.env-row input');
    const shown = await page.locator('.env-row input').nth(1).inputValue();
    assert(shown === '********', 'values are masked after reload, got ' + shown);
    await shot('7-env');
    // logs and CI tabs render
    await page.click('.tabs a:has-text("Logs")');
    await page.waitForSelector('pre.logs');
    await page.click('.tabs a:has-text("CI/CD")');
    await page.waitForSelector('.code pre');
    assert((await page.textContent('.code pre')).includes('ghcr.io/acme/blog'), 'workflow embeds the registered image');
    await shot('8-ci');
    // cron + settings
    await page.goto(BASE + '/#/cron');
    await page.waitForSelector('table tbody tr');
    await shot('9-cron');
    await page.goto(BASE + '/#/settings');
    await page.waitForSelector('.panel h4');
    await shot('10-settings');
    // sign out
    await page.click('aside button.ghost');
    await page.waitForSelector('.login form');

    // The CSP must have blocked nothing and the app must not have thrown.
    assert(errors.length === 0, 'browser errors: ' + errors.join(' | '));
    console.log('UI E2E OK');
  } catch (e) {
    console.error('UI E2E FAILED:', e.message);
    if (errors.length) console.error(errors.join('\n'));
    process.exitCode = 1;
  } finally {
    if (browser) await browser.close();
    srv.kill();
  }
})();
