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
  /** A timer handle the page cleared to null: the platform takes it as no timer. */
  function clearTimeout(id: number | null): void;
  /** An interval handle the page cleared to null: the platform takes it as no timer. */
  function clearInterval(id: number | null): void;
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
    /** Present in Chromium, with the members its own pages and extensions get. */
    chrome?: { webstore?: object; runtime?: object };
    /** Gecko's record of where the viewport sits on the screen. */
    mozInnerScreenX?: number;
    /** Gecko's record of where the viewport sits on the screen. */
    mozInnerScreenY?: number;
    webkitAudioContext?: typeof AudioContext;
  }

  interface Navigator {
    audioSession?: { type: string };
    userAgentData?: { brands?: { brand: string; version: string }[]; platform?: string; mobile?: boolean };
    /** The Keyboard API's lock, which Chromium offers in secure contexts. */
    keyboard?: { lock(keyCodes?: string[]): Promise<void>; unlock(): void };
    /** Present in Brave alone. */
    brave?: object;
  }

  interface Permissions {
    /** The clipboard permissions Chromium names, which the DOM typings leave out. */
    query(permissionDesc: { name: 'clipboard-read' | 'clipboard-write' }): Promise<PermissionStatus>;
  }

  interface KeyboardEvent {
    /** Set on the keydown `lib/clipboard-sync.ts` dispatches again once the transfer it waited on settles. */
    __selkiesClipReplay?: boolean;
  }

  interface Event {
    /** Set by the `Input` that handled the event, under a key naming that instance. */
    [handledBy: `_GUAC_KEYBOARD_HANDLED_BY_${number}`]: boolean | undefined;
  }

  interface WheelEvent {
    /** The legacy wheel delta WebKit and Blink still report beside `deltaX`. */
    wheelDeltaX?: number;
    /** The legacy wheel delta WebKit and Blink still report beside `deltaY`. */
    wheelDeltaY?: number;
  }

  interface Touch {
    /** What made the contact, which iPadOS reports: `direct` for a finger, `stylus` for a pencil. */
    touchType?: string;
  }

  interface FullscreenOptions {
    /** Chromium's request to hold the system keys for the fullscreen it grants. */
    keyboardLock?: string;
  }

  interface HTMLElement {
    virtualKeyboardPolicy?: string;
  }
}
