// Exercise the built, hydrated app against an already running local server.
// Run: QUANT_SITE_URL=http://127.0.0.1:4202 npm run test:browser
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const { chromium } = require('playwright');

(async () => {
  const baseURL = process.env.QUANT_SITE_URL || 'http://127.0.0.1:4202';
  const output = path.resolve(__dirname, '../../test-results');
  await fs.mkdir(output, { recursive: true });
  const browser = await chromium.launch({ timeout: 20000, headless: true, executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE });
  try {
    console.log("Browser launched");
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, colorScheme: 'light', reducedMotion: 'reduce' });
    const page = await context.newPage();
    page.setDefaultTimeout(15000);
    const checkFrames = () => page.waitForFunction(() =>
      getComputedStyle(document.querySelector('#grant > div')).position === 'relative' &&
      getComputedStyle(document.querySelector('#grant svg')).position === 'absolute'
    );
    console.log("Page created");
    const errors = [];
    page.on('pageerror', error => { errors.push(error.message); console.error('pageerror:', error.message); });
    page.on('console', message => { if (message.type() === 'error') { errors.push(message.text()); console.error('console:', message.text()); } });
    const response = await page.goto(baseURL, { waitUntil: 'networkidle' });
    assert.equal(response.status(), 200);
    console.log("HTTP 200");
    for (const endpoint of ['/healthz', '/readyz']) {
      assert.equal((await context.request.get(`${baseURL}${endpoint}`)).status(), 200);
    }
    await page.locator('html[data-hydrated="true"]').waitFor({ timeout: 30000 });
    await checkFrames();
    assert.equal(await page.locator('h1').count(), 1);
    await page.screenshot({ path: path.join(output, 'desktop-hero.png') });
    await page.screenshot({ path: path.join(output, 'desktop-light.png'), fullPage: true });
    await page.locator('#theme-toggle').click();
    await page.waitForFunction(() => document.documentElement.dataset.theme === 'light');
    await checkFrames();
    await page.locator('#theme-toggle').click();
    await page.waitForFunction(() => document.documentElement.dataset.theme === 'dark');
    await checkFrames();
    await page.screenshot({ path: path.join(output, 'desktop-dark.png'), fullPage: true });
    await page.reload({ waitUntil: 'networkidle' });
    await page.locator('html[data-hydrated="true"]').waitFor();
    await page.waitForFunction(() => document.documentElement.dataset.theme === 'dark');
    await checkFrames();
    await page.locator('#faq-0-button').click();
    await page.waitForFunction(() => document.querySelector('#faq-0-button')?.getAttribute('aria-expanded') === 'true');
    assert(await page.locator('#faq-0-answer').isVisible());
    await page.locator('#faq-0-button').click();
    await page.waitForFunction(() => !document.querySelector('#faq-0-answer'));
    const input = page.getByRole('textbox', { name: 'Your correction to the held claim' });
    await input.pressSequentially('Tea only on trains', { delay: 30 });
    assert.equal(await input.inputValue(), 'Tea only on trains');
    await checkFrames();
    for (let i = 0; i < 'Tea only on trains'.length; i++) await input.press('ArrowLeft');
    await input.pressSequentially('Hot ', { delay: 30 });
    assert.equal(await input.inputValue(), 'Hot Tea only on trains');
    await input.press('Delete');
    await input.pressSequentially('t');
    assert.equal(await input.inputValue(), 'Hot tea only on trains');
    await input.press('ControlOrMeta+A');
    await input.pressSequentially('Tea only on trains', { delay: 30 });
    assert.equal(await input.inputValue(), 'Tea only on trains');
    await page.getByRole('button', { name: 'Correct this', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('#claim-fixed-text')?.textContent.includes('Tea only on trains'));
    await page.locator('#nav-links').getByRole('link', { name: 'Privacy', exact: true }).click();
    await page.waitForURL('**/privacy');
    assert((await page.locator('h1').textContent()).includes('plain language'));
    const privacy = await context.request.get(`${baseURL}/privacy`);
    assert.equal(privacy.status(), 200);
    assert((await privacy.text()).includes('Where it lives'));
    const missing = await context.request.get(`${baseURL}/not-a-page`);
    assert.equal(missing.status(), 404);
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(baseURL, { waitUntil: 'networkidle' });
    await page.locator('html[data-hydrated="true"]').waitFor();
    await page.locator('#menu-toggle').click();
    await page.waitForFunction(() => document.querySelector('#menu-toggle')?.getAttribute('aria-expanded') === 'true');
    assert(await page.locator('#nav-menu').isVisible());
    await page.locator('#menu-toggle').click();
    await page.waitForFunction(() => !document.querySelector('#nav-menu'));
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'no mobile horizontal overflow');
    await page.screenshot({ path: path.join(output, 'mobile-dark-hero.png') });
    await page.locator('#permissions').screenshot({ path: path.join(output, 'mobile-dark-agents.png') });
    await page.screenshot({ path: path.join(output, 'mobile.png'), fullPage: true });
    await page.locator('#theme-toggle').click();
    await page.locator('#theme-toggle').click();
    await page.waitForFunction(() => document.documentElement.dataset.theme === 'light');
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.screenshot({ path: path.join(output, 'mobile-light-hero.png') });
    assert.deepEqual(errors, []);
    console.log('PASS: hydration, themes/persistence, FAQ, correction demo, routes, 404, menu, mobile layout');
    console.log(`Screenshots: ${output}`);
    await context.close();
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
