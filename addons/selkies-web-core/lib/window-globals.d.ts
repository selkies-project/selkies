/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The `window` state the two streaming cores publish for the dashboards and
 * for each other, and the engine properties they probe that the DOM typings
 * do not carry. A property is optional where the cores read it before they
 * write it, and typed `any` where it holds a server setting or a counter the
 * cores fill from untyped messages.
 * @module
 */
export {};

declare global {
  /** Chromium's insertable-streams track source, absent from the DOM typings. */
  var MediaStreamTrackGenerator: any;

  interface Window {
    /** Server settings applied by name: the cores write `window[key]` for each setting the table names. */
    [key: string]: any;
    webrtcInput?: any;
    selkiesTransport?: any;
    selkiesLogs?: { log: any[]; debug: any[] };
    applyTimestamp?: (msg: any) => any;
    isCleaningUp?: boolean;
    isFallingBack?: boolean;
    isAudioInitializing?: boolean;
    manual_resolution?: boolean;
    manual_width?: any;
    manual_height?: any;
    manualResolution?: boolean;
    enable_resize?: boolean;
    clipboard_enabled?: boolean;
    video_fullcolor?: boolean;
    video_10bit?: boolean;
    video_bitrate?: any;
    video_crf?: any;
    audio_channels?: any;
    displayRefreshRate?: number | null;
    streamResolutionDiverged?: boolean;
    videoChunksReceived: number;
    videoStripeRows?: Record<string, any>;
    videoDivertOn?: boolean;
    currentAudioBufferDuration: number;
    currentAudioDropped: number;
    currentAudioUnderrunSamples: number;
    currentAudioWorkletDropped: number;
    currentAudioLevel: number;
    chrome?: any;
    webkitAudioContext?: typeof AudioContext;
  }

  interface Navigator {
    audioSession?: { type: string };
    userAgentData?: { brands?: { brand: string; version: string }[]; platform?: string; mobile?: boolean };
  }

  interface HTMLElement {
    virtualKeyboardPolicy?: string;
  }
}
