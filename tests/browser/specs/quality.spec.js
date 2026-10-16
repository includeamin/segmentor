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

// KNOWN: what these specs found, and have left failing on purpose (see docs/conformance.md).
// The ladder's manifest puts every video rendition in an AdaptationSet of its own, so dash.js sees
// one rendition, not a ladder, and can never switch. The entry must go once that is fixed: the test
// then passes, and an expected failure that passes fails the run.
const KNOWN =
  'the DASH manifest has one AdaptationSet per video rendition, so dash.js lists a single rendition and cannot switch between them';

async function levelsOf(page) {
  await expect.poll(() => page.evaluate(() => { try { return window.harness.levels().length; } catch { return 0; } })).toBeGreaterThanOrEqual(2);
  const levels = await page.evaluate(() => window.harness.levels());
  const byHeight = levels.map((level, index) => ({ index, height: level.height })).sort((a, b) => a.height - b.height);
  return { low: byHeight[0], high: byHeight[byHeight.length - 1] };
}

for (const player of Object.keys(PLAYERS)) {
  for (const asset of ASSETS) {
    for (const [name, start] of [['up then down', 'lowest'], ['down then up', 'highest']]) {
      test(`${player} switches ${name} on ${asset}`, async ({ page }) => {
        if (player === 'dash') test.fail(true, KNOWN);
        await play(page, player, asset, start);
        const { low, high } = await levelsOf(page);
        expect(high.height, 'the renditions differ in size').toBeGreaterThan(low.height);
        const [first, second] = start === 'lowest' ? [high, low] : [low, high];
        const initial = start === 'lowest' ? low : high;

        await reach(page, 0.2);
        await expect
          .poll(async () => (await state(page)).height, { message: `it starts ${initial.height} high`, intervals: FAST })
          .toBe(initial.height);

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
