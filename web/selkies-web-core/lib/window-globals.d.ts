/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The `window` state the two streaming cores publish for the dashboards and
 * for each other, and the engine properties they probe that the DOM typings
 * do not carry. A property is optional where the cores read it before they
 * write it; a server setting the cores apply by name is `unknown` until read.
 * @module
 */
import type { Input } from './input.ts';
import type { StreamClient, StreamInfo, StreamStatsWindow } from './stream-stats.ts';

declare global {
  /** A timer handle the page cleared to null: the platform takes it as no timer. */
  function clearTimeout(id: ReturnType<typeof setTimeout> | null): void;
  /** An interval handle the page cleared to null: the platform takes it as no timer. */
  function clearInterval(id: ReturnType<typeof setInterval> | null): void;
  /** The track an insertable-streams reader takes, which the worker typings leave out. */
  interface MediaStreamTrackProcessorInit {
    track: MediaStreamTrack;
  }

  /** Chromium's insertable-streams track reader, declared as the worker typings declare it for the page as well. */
  interface MediaStreamTrackProcessor {
    readonly readable: ReadableStream;
  }

  var MediaStreamTrackProcessor: {
    prototype: MediaStreamTrackProcessor;
    new(init: MediaStreamTrackProcessorInit): MediaStreamTrackProcessor;
  };

  interface RTCInboundRtpStreamStats {
    /** Whether the engine counts the decoder in use as power efficient, which Chromium reports. */
    powerEfficientDecoder?: boolean;
  }

  interface RTCRtpEncodingParameters {
    /** The codec this encoding sends with, where the engine lets a sender choose among the negotiated ones. */
    codec?: RTCRtpCodec;
  }

  interface AudioContext {
    /** Routes the context's output to a device, where the engine has it. */
    setSinkId(sinkId: string): Promise<void>;
  }

  interface VideoEncoderConfig {
    /** The H.265 bitstream format, where the platform encodes it. */
    hevc?: { format: 'annexb' | 'hevc' };
  }

  /** Chromium's insertable-streams track source, absent from the DOM typings. */
  var MediaStreamTrackGenerator: { new (init: { kind: 'video' | 'audio' }): MediaStreamTrack & { writable: WritableStream<VideoFrame> } };

  interface Window {
    /** Server settings applied by name: the cores write `window[key]` for each setting the table names. */
    [key: string]: unknown;
    /** The input handler of the page's stream. */
    webrtcInput?: Input | null;
    /** The WebSockets core's session socket, for the suites. */
    selkiesTransport?: object | null;
    /** The frame rate the page presents, as the cores measure it. */
    fps: number;
    /** What the server said of the stream (`lib/stream-stats.ts`). */
    stream_info?: StreamInfo | null;
    /** This page's half of the stream. */
    stream_client?: StreamClient | null;
    /** The stream's samples while the stats are open. */
    stream_stats?: StreamStatsWindow;
    /** The audio the playback path holds, as the WebSockets core counts it. */
    currentAudioBufferSize?: number;
    /** Set by the dashboard around a transport switch, so the active core leaves the old peer's teardown to the reload the switch makes. */
    __selkiesModeSwitching?: boolean;
    selkiesLogs?: { log: string[]; debug: string[] };
    applyTimestamp?: (msg: unknown) => string;
    isCleaningUp?: boolean;
    isFallingBack?: boolean;
    isAudioInitializing?: boolean;
    manual_resolution?: boolean;
    manual_width?: number | null;
    manual_height?: number | null;
    manualResolution?: boolean;
    enable_resize?: boolean;
    clipboard_enabled?: boolean;
    video_fullcolor?: boolean;
    video_10bit?: boolean;
    video_bitrate?: number;
    video_crf?: number;
    audio_channels?: number | string;
    displayRefreshRate?: number | null;
    streamResolutionDiverged?: boolean;
    videoChunksReceived: number;
    videoStripeRows?: Record<string, number>;
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

  /**
   * A message a dashboard posts to the page for the core: `type` names the
   * command, and the other members are the ones that command reads. `value`
   * is whatever the command carries, so a handler narrows it before use.
   */
  interface DashboardMessage {
    type?: string;
    value?: unknown;
    enabled?: boolean;
    pipeline?: string;
    settings?: Record<string, unknown>;
    deviceId?: string;
    context?: string;
    imageBlob?: Blob;
    text?: string;
    url?: string;
    width?: number | string;
    height?: number | string;
    open?: boolean;
    persist?: boolean;
    audio?: boolean;
    video?: boolean;
    microphone?: boolean;
    gamepad?: boolean;
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
