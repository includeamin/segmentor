// Shared by the specs: how each player is pointed at an asset, and what a spec reads from the page.
import { expect } from '@playwright/test';

export const PLAYERS = {
  hls: (asset) => `${process.env.VODNIK_URL}/hls/${asset}/master.m3u8`,
  dash: (asset) => `${process.env.VODNIK_URL}/dash/${asset}/manifest.mpd`,
};

/**
 * The assets of tests/fixtures/harness-catalog.json that a player can play, and how many clips
 * each is made of. Every join between clips is a place a player can stumble.
 */
export const ASSETS = { single: 1, ladder: 1, preroll: 2, grid: 2, trimmed: 1 };

/** Opens the page and starts `player` on `asset`, on the rendition `start` ('lowest' or 'highest'). */
export async function play(page, player, asset, start = 'lowest') {
  await page.goto(`${process.env.PAGE_URL}/`);
  await page.evaluate(
    ([kind, url, start]) => window.harness.load(kind, url, start),
    [player, PLAYERS[player](asset), start],
  );
}

export const state = (page) => page.evaluate(() => window.harness.state());
export const errors = (page) => page.evaluate(() => window.harness.errors);

/**
 * How often a wait looks. The assets last a few seconds and a switch between renditions only shows
 * at the next segment boundary, so a wait that backs off to a second would miss the moment to act.
 */
export const FAST = [50, 100, 250];

/**
 * Runs `wait`, and if it times out says where playback was and what the player had reported, so a
 * failure explains itself instead of only saying that a number never arrived.
 */
async function explained(page, what, wait) {
  try {
    await wait();
  } catch (error) {
    error.message += `\nwhile waiting for ${what}: the page was ${JSON.stringify(await state(page))}, player errors ${JSON.stringify(await errors(page))}`;
    throw error;
  }
}

/** Waits until playback has reached `seconds`. */
export async function reach(page, seconds, timeout = 30_000) {
  await explained(page, `playback to reach ${seconds} s`, () =>
    expect.poll(async () => (await state(page)).time, { timeout, intervals: FAST }).toBeGreaterThanOrEqual(seconds));
}

/** Waits until playback is within `slack` seconds of the end, or has ended. */
export async function reachNearEnd(page, slack = 0.06, timeout = 30_000) {
  await explained(page, 'playback to get to the end', () =>
    expect
      .poll(async () => {
        const { everEnded, time, duration } = await state(page);
        return everEnded || time >= duration - slack;
      }, { timeout, intervals: FAST })
      .toBe(true));
}

export async function reachEnd(page, timeout = 30_000) {
  await explained(page, 'playback to end', () =>
    expect.poll(async () => (await state(page)).everEnded, { timeout, intervals: FAST }).toBe(true));
}

/** The presentation's length according to the manifest, in seconds. */
export async function manifestDuration(request, asset) {
  const response = await request.get(PLAYERS.dash(asset));
  expect(response.ok()).toBe(true);
  const match = (await response.text()).match(/mediaPresentationDuration="PT([\d.]+)S"/);
  return Number(match[1]);
}
