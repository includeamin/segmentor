import { test, expect } from '@playwright/test';
import { FAST, PLAYERS, errors, play, reach, state } from './helpers.js';

// Switching quality is what an adaptive stream is for, and the join between two clips is where it
// is most likely to break: the other rendition has to be cut at the same instants, have its own
// init segment for the new clip, and carry on from the same time. Both renditions are forced in
// turn and each switch has to show: the picture changes size.
//
// A player replaces what is buffered after the fragment it is playing, so a forced switch shows at
// the next segment boundary. Each switch therefore waits for the one before it to show. The first
// lands at the join between the two clips of `grid`, which is the point of it.
const ASSETS = ['grid', 'ladder'];

// KNOWN: what these specs found, and have left unexplained (docs/conformance.md). On `grid`, after
// a switch up from the lowest rendition, dash.js reports the higher one for about half a second and
// then goes back to the lowest on its own, so the picture never gets larger. The same switch on
// `ladder`, and a switch down on either, work. A listed case checks that symptom and nothing else;
// it fails once the picture does grow, and the entry must go.
const KNOWN = {
  'dash switches up then down on grid':
    'dash.js goes back to the lowest rendition of an adaptive-clips asset about half a second after a forced switch up',
};

async function levelsOf(page) {
  await expect.poll(() => page.evaluate(() => { try { return window.harness.levels().length; } catch { return 0; } }), { intervals: FAST }).toBeGreaterThanOrEqual(2);
  const levels = await page.evaluate(() => window.harness.levels());
  const byHeight = levels.map((level, index) => ({ index, height: level.height })).sort((a, b) => a.height - b.height);
  return { low: byHeight[0], high: byHeight[byHeight.length - 1] };
}

for (const player of Object.keys(PLAYERS)) {
  for (const asset of ASSETS) {
    for (const [name, start] of [['up then down', 'lowest'], ['down then up', 'highest']]) {
      test(`${player} switches ${name} on ${asset}`, async ({ page }) => {
        const known = KNOWN[`${player} switches ${name} on ${asset}`];
        await play(page, player, asset, start);
        const { low, high } = await levelsOf(page);
        expect(high.height, 'the renditions differ in size').toBeGreaterThan(low.height);
        const [first, second] = start === 'lowest' ? [high, low] : [low, high];
        const initial = start === 'lowest' ? low : high;

        await reach(page, 0.2);
        await expect
          .poll(async () => (await state(page)).height, { message: `it starts ${initial.height} high`, intervals: FAST })
          .toBe(initial.height);

        if (known) {
          await page.evaluate((index) => window.harness.setLevel(index), first.index);
          await reach(page, (await state(page)).time + 2);
          expect((await state(page)).height, `the known problem is gone, so remove its entry from KNOWN: ${known}`).toBe(initial.height);
          expect(await errors(page)).toEqual([]);
          return;
        }
        for (const target of [first, second]) {
          await page.evaluate((index) => window.harness.setLevel(index), target.index);
          await expect
            .poll(async () => (await state(page)).height, { message: `the picture becomes ${target.height} high`, intervals: FAST })
            .toBe(target.height);
        }

        // Playback carries on after the last switch. Whether the stream then ends is the playback
        // spec's business: at a join between clips, hls.js has a known problem with that.
        const { time } = await state(page);
        await reach(page, time + 0.4);
        expect(await errors(page)).toEqual([]);
      });
    }
  }
}
