import { test, expect } from '@playwright/test';
import { ASSETS, FAST, PLAYERS, errors, manifestDuration, play, reach, reachEnd, reachNearEnd, state } from './helpers.js';

// What a viewer does: press play, let it run, jump around. Each is checked for every kind of
// asset the server can serve, under both players, and none may raise a player error.
//
// KNOWN: what these specs found, and have left unfixed on purpose. A listed test checks that the
// specific symptom is still there, and passes only then: any other failure (it does not start, a
// player error, a fatal 404) still fails the run, and so does the symptom going away, when the
// entry must go. A blanket expected failure would absorb every one of those. Evidence and what was ruled out are in docs/conformance.md ("Found by the browser suite").
const KNOWN = {
  'hls preroll ends':
    'hls.js never fires `ended` on a sequence of files: it stops 35 ms short of the end with a bufferStalledError, and its main stream never reaches ENDED',
  'dash preroll plays all of it':
    'dash.js skips about 0.3 s of a sequence at the join between clips (141 of 150 frames are presented)',
  'dash grid plays all of it':
    'dash.js skips about 0.3 s at the join between clips of an adaptive-clips asset (112 of 118 frames are presented)',
};

// KNOWN, but not every time: these fail in a fraction of runs (hls.js 1.7.3 in Chrome 155), so
// they cannot gate anything. They are tagged @intermittent and left out unless INTERMITTENT=1 is
// set; with it they run, to measure the rate or to see whether a fix worked. Retrying them would
// only hide a player that sometimes does not finish.
const INTERMITTENT = new Set(['hls grid ends', 'hls grid plays all of it']);
const tag = (player, asset, name) => (INTERMITTENT.has(`${player} ${asset} ${name}`) ? ' @intermittent' : '');

const knownReason = (player, asset, name) => KNOWN[`${player} ${asset} ${name}`];
const gone = (reason) => `the known problem is gone, so remove its entry from KNOWN: ${reason}`;

for (const player of Object.keys(PLAYERS)) {
  for (const [asset, clips] of Object.entries(ASSETS)) {
    test.describe(`${player} plays ${asset}`, () => {
      test('starts', async ({ page }) => {
        await play(page, player, asset);
        await reach(page, 0.5);
        expect(await errors(page)).toEqual([]);
      });

      test(`ends for the length the manifest states${tag(player, asset, 'ends')}`, async ({ page, request }) => {
        const known = knownReason(player, asset, 'ends');
        const expected = await manifestDuration(request, asset);
        await play(page, player, asset);
        if (known) {
          // The symptom: it gets to the end and never says so.
          await reachNearEnd(page);
          await page.waitForTimeout(3000);
          expect((await state(page)).everEnded, gone(known)).toBe(false);
          return;
        }
        await reachEnd(page);

        const end = await state(page);
        expect(Math.abs(end.duration - expected), `duration ${end.duration} against ${expected}`).toBeLessThan(0.2);
        expect(await errors(page)).toEqual([]);
      });

      test(`plays all of it${tag(player, asset, 'plays all of it')}`, async ({ page }) => {
        const known = knownReason(player, asset, 'plays all of it');
        await play(page, player, asset);
        await reach(page, 0.5);
        const { duration } = await state(page);
        await reachNearEnd(page);

        // What was played covers the presentation. What may be missing is explained: the video
        // starts 67 ms in, the last 60 ms is not waited for, and each join between clips can leave
        // one reorder delay (67 ms). Anything more is content the viewer did not see.
        const { played } = await state(page);
        const covered = played.reduce((total, [from, to]) => total + (to - from), 0);
        const allowed = 0.15 + 0.08 * (clips - 1);
        expect(await errors(page)).toEqual([]);
        const missing = duration - covered;
        if (known) {
          expect(missing, gone(known)).toBeGreaterThanOrEqual(allowed);
          return;
        }
        expect(missing, `played ${JSON.stringify(played)} of ${duration}`).toBeLessThan(allowed);
      });

      test('seeks to the middle and near the end', async ({ page }) => {
        await play(page, player, asset);
        await reach(page, 0.3);
        const { duration } = await state(page);

        // The assets last a few seconds, so playing through a seek would race the end of the
        // stream. Each seek is made while paused, and playback resumed from there.
        for (const target of [duration * 0.6, duration - 0.5]) {
          await page.evaluate(() => window.harness.pause());
          await page.evaluate((seconds) => window.harness.seek(seconds), target);
          // The seek has to land where it was sent, not merely be followed by playback: a short
          // asset reaches any target soon enough on its own.
          await expect
            .poll(async () => Math.abs((await state(page)).time - target), { message: `the seek lands on ${target}`, intervals: FAST })
            .toBeLessThan(0.25);
          await page.evaluate(() => window.harness.resume());
          await reach(page, target + 0.2);
        }
        await reachNearEnd(page);
        expect(await errors(page)).toEqual([]);
      });
    });
  }
}
