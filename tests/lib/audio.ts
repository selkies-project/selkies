/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The test tone the audio suites play and listen for, and how much of a
 * recording is that tone.
 * @module
 */

import { writeFileSync } from 'node:fs';

/** The capture rate, the tone's sample rate too. */
export const CAPTURE_RATE = 48_000;
/** The tone's frequency. */
export const TONE_HZ = 1000;

/**
 * Write a mono 16-bit WAV of the tone.
 * @param path Where.
 * @param seconds How long.
 * @param amplitude Its peak.
 */
export function toneWav(path: string, seconds = 2, amplitude = 11_000): void {
    const n = CAPTURE_RATE * seconds;
    const wav = Buffer.alloc(44 + n * 2);
    wav.write('RIFF', 0, 'ascii');
    wav.writeUInt32LE(36 + n * 2, 4);
    wav.write('WAVEfmt ', 8, 'ascii');
    wav.writeUInt32LE(16, 16);
    wav.writeUInt16LE(1, 20);
    wav.writeUInt16LE(1, 22);
    wav.writeUInt32LE(CAPTURE_RATE, 24);
    wav.writeUInt32LE(CAPTURE_RATE * 2, 28);
    wav.writeUInt16LE(2, 32);
    wav.writeUInt16LE(16, 34);
    wav.write('data', 36, 'ascii');
    wav.writeUInt32LE(n * 2, 40);
    for (let i = 0; i < n; i++) {
        // As Python's int(): toward zero.
        wav.writeInt16LE(Math.trunc(amplitude * Math.sin(2 * Math.PI * TONE_HZ * i / CAPTURE_RATE)), 44 + i * 2);
    }
    writeFileSync(path, wav);
}

/** How loud a recording is and how much of it is the tone. */
export interface Tone {
    samples: number;
    rms: number;
    tone: number;
    ratio: number;
}

/**
 * RMS of the recording and how much of it is the tone at `hz`. A Goertzel
 * filter at `hz` measures the tone's power in each 20 ms block of the last
 * second; against the RMS that gives the fraction of the signal that is the
 * tone, close to 1 for the tone alone and near 0 for silence or noise.
 * Blocks, not one filter over the second: the microphone path cuts audio
 * standing in its queue to hold its latency, which steps the tone's phase,
 * and a filter spanning the cut adds both sides out of step.
 * @param samples The recording, 16-bit at the capture rate.
 * @param hz The tone.
 * @returns The figures.
 */
export function analyze(samples: Int16Array, hz = TONE_HZ): Tone {
    const block = CAPTURE_RATE / 50;
    const n = Math.floor(Math.min(samples.length, CAPTURE_RATE) / block) * block;
    if (n < CAPTURE_RATE / 4) return { samples: samples.length, rms: 0, tone: 0, ratio: 0 };
    const window = samples.subarray(samples.length - n);
    const coeff = 2 * Math.cos(2 * Math.PI * hz / CAPTURE_RATE);
    let energy = 0;
    let tonePower = 0;
    for (let start = 0; start < n; start += block) {
        let s1 = 0;
        let s2 = 0;
        for (let i = start; i < start + block; i++) {
            const x = window[i];
            const s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
            energy += x * x;
        }
        tonePower += 2 * Math.max(s1 * s1 + s2 * s2 - coeff * s1 * s2, 0) / (block * block);
    }
    const rms = Math.sqrt(energy / n);
    const toneRms = Math.sqrt(tonePower / (n / block));
    const round = (v: number, places: number): number => Math.round(v * 10 ** places) / 10 ** places;
    return { samples: samples.length, rms: round(rms, 1), tone: round(toneRms, 1), ratio: rms ? round(toneRms / rms, 3) : 0 };
}
