/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// 8. Test `#shared` and `#player*`. For the shared window, only the
// screen/audio should work and nothing else. Check no long-term screen quality
// degradations. For controllers, the gamepad should also work.
//
// A `#shared` page beside the controller's must show the screen and play the
// session's sound, and must not move the session's pointer, type into it, put
// its clipboard into the session, offer uploads, or drive a gamepad. Its
// picture of a still, detailed screen must not get worse over the run (the
// error against the known picture, sampled over `E2E_IMAGE_DRIFT_SECONDS`,
// default 120; set it to tens of minutes for the long check). `#player2` to
// `#player4` must each drive a pad the session's tester sees.
import { fields, numberOr, stringOr } from '../../../web/selkies-web-core/lib/narrow.ts';
import { sleep } from '../../lib/env.ts';
import { padInitScript } from '../../lib/uinput.ts';
import { audioLevel, patternMatches, sessionBrowser, stopSessionBrowser, type Cell } from '../lib.ts';

export const ITEM = 8;
export const TITLE = '#shared viewer and #player controllers';
const DRIFT = Number(process.env.E2E_IMAGE_DRIFT_SECONDS ?? '120');

const pressedOf = (pad: unknown): number[] => {
    const p = fields(pad).pressed;
    return Array.isArray(p) ? p.map(Number) : [];
};

export async function run(cell: Cell): Promise<void> {
    const R = cell.res;
    const owner = await cell.open();
    let shared = await cell.open('#shared', { init: [padInitScript()] });
    const s = await cell.waitPattern(shared, 30);
    R.check("#shared shows the session's screen", patternMatches(s), s?.points ?? null);
    await cell.go('/tone.html');
    const level = await audioLevel(shared, 10);
    R.check("#shared plays the session's sound", level > 0, `level ${level}`);
    await cell.go('/pattern.html');
    R.check('#shared renders no dashboard and no upload control',
        (await shared.locator('button:has-text("Upload Files")').count()) === 0 && (await shared.locator('.sidebar').count()) === 0, '');

    // Input from the viewer must not reach the session; the owner's must (the control).
    // The session's pattern page counts the pointer moves its application gets.
    // Each page is clicked first, as a user does on switching to it: a page without
    // focus may send nothing, which would pass the viewer's check for no reason.
    const moves = async (): Promise<number> => numberOr((await cell.report('pointer', 5))?.moves) ?? 0;
    await shared.bringToFront();
    await shared.mouse.click(400, 400);
    await sleep(1000);
    const before = await moves();
    for (const [x, y] of [[200, 200], [900, 500], [300, 600]]) {
        await shared.mouse.move(x, y);
        await sleep(300);
    }
    await shared.mouse.click(400, 400);
    await sleep(1500);
    const after = await moves();
    await owner.bringToFront();
    await owner.mouse.click(611, 333);
    await owner.mouse.move(640, 360);
    await owner.mouse.move(700, 400);
    await sleep(1500);
    const moved = await moves();
    R.check("the viewer's pointer does not move the session's, the owner's does", after === before && moved > after,
        `moves ${before} -> ${after} (viewer) -> ${moved} (owner)`);
    await cell.go('/ime.html');
    await sleep(2000);
    await shared.bringToFront();
    await shared.mouse.click(640, 360);
    await shared.keyboard.type('viewer');
    await sleep(1500);
    const typed = stringOr((await cell.report('ime', 5))?.value) ?? '';
    R.check("the viewer's keys do not reach the session", !typed.includes('viewer'), JSON.stringify(typed.slice(-40)));
    await cell.go('/pattern.html');

    // The viewer's clipboard stays out of the session.
    const tools = Array.isArray(cell.facts.tools) ? cell.facts.tools.map(String) : [];
    if ((cell.engine === 'chromium' || cell.engine === 'firefox') && tools.includes('xclip')) {
        await cell.target.sh('printf session-own | (setsid xclip -selection clipboard -i > /dev/null 2>&1 &)');
        await sleep(1000);
        await cell.xclient(['xclip', '-selection', 'clipboard', '-i'], Buffer.from('viewer-clip'));
        await shared.bringToFront();
        await shared.mouse.click(640, 360);
        await shared.keyboard.press('Control+v');
        await sleep(3000);
        const clip = await cell.target.out('timeout 5 xclip -selection clipboard -o 2>/dev/null');
        R.check("the viewer's clipboard does not reach the session", !clip.includes('viewer-clip'), JSON.stringify(clip.slice(0, 40)));
    } else {
        R.skip("the viewer's clipboard", 'needs a real client clipboard and xclip in the session');
    }

    // Gamepads: the viewer's pad is refused, a controller's reaches the session.
    await sessionBrowser(cell.target, { start: '/gamepad.html', kiosk: false, profile: 'pad' });
    await sleep(4000);
    await cell.clearReport('gamepad');
    for (const i of [0, 1, 2]) {
        await shared.evaluate(`window.__padPress(${i}, 1)`);
        await sleep(200);
        await shared.evaluate(`window.__padPress(${i}, 0)`);
    }
    await sleep(2000);
    const seen = (await cell.report('gamepad', 5)) ?? {};
    R.check("the viewer's gamepad does not reach the session", !Object.values(seen).some((p) => pressedOf(p).length > 0), seen);
    await shared.close();
    for (const n of [2, 3, 4]) {
        await cell.go('/gamepad.html', 'pad');
        await sleep(3000);
        await cell.clearReport('gamepad');
        const player = await cell.open(`#player${n}`, { init: [padInitScript()] });
        await sleep(2000);
        for (const i of [0, 3, 12]) {
            await player.evaluate(`window.__padPress(${i}, 1)`);
            await sleep(200);
            await player.evaluate(`window.__padPress(${i}, 0)`);
            await sleep(150);
        }
        await sleep(2000);
        const pads = (await cell.report('gamepad', 5)) ?? {};
        const pressed = new Set(Object.values(pads).flatMap(pressedOf));
        R.check(`#player${n}'s gamepad reaches the session`, [0, 3, 12].every((i) => pressed.has(i)),
            Object.fromEntries(Object.entries(pads).map(([k, p]) => [k, [(stringOr(fields(p).id) ?? '').slice(0, 30), fields(p).pressed]])));
        await player.close();
    }
    await stopSessionBrowser(cell.target, 'chrome', 'pad');

    // Long-term quality on a still, detailed screen, as the viewer sees it, with
    // no other page decoding beside it (headless WebKit wedges on two).
    await owner.close();
    await cell.go('/texture.html');
    shared = await cell.open('#shared');
    await sleep(8000);
    const errors: (number | null)[] = [await cell.textureError(shared)];
    for (const until = Date.now() + DRIFT * 1000; Date.now() < until;) {
        await sleep(Math.min(15_000, Math.max(1000, until - Date.now())));
        errors.push(await cell.textureError(shared));
    }
    const valid = errors.filter((e): e is number => e !== null);
    // A sample caught mid-refresh is not drift: the later samples' median and the last one decide.
    const later = valid.slice(1).sort((a, b) => a - b);
    R.check(`the viewer's picture holds its quality over ${DRIFT.toFixed(0)} s`,
        valid.length >= 2 && later[Math.floor(later.length / 2)] <= valid[0] + 1.0 && valid[valid.length - 1] <= valid[0] + 1.0,
        `error ${JSON.stringify(valid)}`);
    await shared.close();
    await cell.go('/pattern.html');
}
