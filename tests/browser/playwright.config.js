import { defineConfig } from '@playwright/test';

// Plays vodnik's output in a real Chrome (docs/technical-design/0017-verification-harness.md).
// The Chrome channel, not Playwright's bundled Chromium: the bundled build has no H.264 or AAC.
// Locally: install Google Chrome. On a runner: `npx playwright install --with-deps chrome`.
export default defineConfig({
  testDir: './specs',
  // Tests tagged @intermittent fail in a fraction of runs (see specs/playback.spec.js). They run
  // only when asked: INTERMITTENT=1 npx playwright test
  grepInvert: process.env.INTERMITTENT ? undefined : /@intermittent/,
  globalSetup: './global-setup.js',
  timeout: 60_000,
  expect: { timeout: 20_000 },
  // A flaky test is a bug to fix, not to retry.
  retries: 0,
  workers: 2,
  reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
  use: {
    channel: 'chrome',
    headless: true,
    launchOptions: { args: ['--autoplay-policy=no-user-gesture-required'] },
    trace: 'retain-on-failure',
  },
});
