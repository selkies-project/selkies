/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// The microphone uplink end to end: a browser's microphone, switched on
// through the dashboard's pipeline control, must bring up the
// SelkiesVirtualMic on the server and feed it -- over the WebSocket (0x02
// Opus frames) and over WebRTC (the audio track on the reserved sendonly
// transceiver) -- through the shared in-process sound-server control plane:
// no `pactl` process is forked on either transport. Chromium's fake audio
// device stands in for a microphone.
import { test } from '@playwright/test';

import { Checks } from '../lib/checks.ts';
import { suiteOf } from '../lib/env.ts';
import { Host } from '../lib/host.ts';
import {
    defaultSource, launchMic as launch, pcmfluxPlaybackSink, sinkIndex, statuses, toggle, unloadLeftoverVirtualMic, VIRTUAL_MIC, virtualMicSources, waitFor, waitStatus,
} from '../lib/microphone.ts';
import { waitWrVideo, waitWsVideo } from '../lib/stream.ts';

async function transportBlock(host: Host, res: Checks, mode: string): Promise<void> {
    unloadLeftoverVirtualMic();
    try {
        const { url } = await host.start({ label: mode, mode });
        const [browser, page, errors] = await launch(url, mode);
        try {
            const video = mode === 'websockets' ? await waitWsVideo(page) : await waitWrVideo(page);
            res.check('stream up', Boolean(video), JSON.stringify(video).slice(0, 100));
            if (mode === 'webrtc' && video) res.check('audio track negotiated', (video.audio ?? 0) >= 1, video.audio ?? null);
            if (mode === 'webrtc') {
                // The WebRTC audio start validates the capture device through
                // the shared control plane before pcmflux opens it.
                res.check('wr: capture device resolved in-process', await host.saw("Configured audio device 'output.monitor' is valid.", 15), '');
            } else {
                res.check('ws: mic control connected at the handshake', await host.saw('Sound server control ready for the microphone (libpulse)', 10), '');
            }
            await toggle(page, true);
            res.check('client reports the microphone active', await waitStatus(page, true), JSON.stringify(await statuses(page)));
            res.check('virtual mic provisioned (server log)', await host.saw(`Virtual microphone '${VIRTUAL_MIC}' is ready`, 20), '');
            // Either router verdict: already on the monitor, or moved there.
            res.check('pcmflux capture routing verified in-process', (await host.saw("pcmflux correctly connected to 'output.monitor'", 20)) || (await host.saw("moving it to 'output.monitor'", 1)), '');
            res.check('virtual mic source appears', await waitFor(() => virtualMicSources().length > 0, 10), virtualMicSources());
            res.check('virtual mic is the default source', await waitFor(() => defaultSource().includes(VIRTUAL_MIC), 10), defaultSource());
            const inputSink = sinkIndex('input');
            res.check("pcmflux mic playback feeds the 'input' sink", await waitFor(() => pcmfluxPlaybackSink() === inputSink && inputSink !== null, 15), [pcmfluxPlaybackSink(), inputSink]);
            await toggle(page, false);
            res.check('client reports the microphone inactive', await waitStatus(page, false), JSON.stringify(await statuses(page)));
            res.check('no page errors', errors.length === 0, errors.join('; ').slice(0, 200));
        } finally {
            await browser.close();
        }
        if (mode === 'websockets') {
            // The socket that loaded the module unloads it when it goes away.
            res.check('ws: module unloaded on disconnect', (await host.saw('Unloading PulseAudio module', 10)) && (await waitFor(() => virtualMicSources().length === 0, 10)), virtualMicSources());
        }
        res.check('no pactl fallback engaged', !(await host.saw('pactl', 1)), '');
        res.check('no sound-server control failures', !(await host.saw('Sound server control failed', 1)) && !(await host.saw('Sound server did not answer', 1)), '');
    } finally {
        await host.stop();
    }
    if (mode === 'webrtc') {
        // The service unloads its module in the graceful shutdown SIGTERM triggers.
        res.check('webrtc: module unloaded at shutdown', await waitFor(() => virtualMicSources().length === 0, 10), virtualMicSources());
    }
    unloadLeftoverVirtualMic();
}

for (const mode of ['websockets', 'webrtc']) {
    test(mode, async () => {
        const res = new Checks(`microphone-${mode}`);
        const host = new Host(suiteOf(import.meta.filename));
        try {
            await transportBlock(host, res, mode);
        } finally {
            await host.close();
        }
        res.finish();
    });
}
