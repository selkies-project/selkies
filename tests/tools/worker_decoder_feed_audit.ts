/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * When the page's feed to the video worker's decoder asks for a key frame.
 * Every key frame the encoder codes at a constrained rate is a picture the
 * viewer sees blur, so the feed asks for one only where the decoder has
 * nothing to start from: a configure brought on by a key frame is started by
 * that key frame, and one brought on by a delta frame is not. In avcC framing
 * a delta frame cannot configure at all, since the decoder's description
 * comes from a key frame's parameter sets, so it is dropped behind the
 * request. The first frame handed on starts the stream, which ends the key
 * frame requests the page makes while it waits for a first frame.
 *
 * `feedWorkerDecoder` is taken from the WebSockets core as the browser runs
 * it and driven over stand-ins for the worker and the page state it reads;
 * each case counts the configures and chunks it posts, the key frame
 * requests it sends, and the times it starts the stream.
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 * @module
 */
import vm from 'node:vm';
import { CORE, declaration, readSource } from './core_source.ts';

let failed = 0;

/** Prints one check's line and counts a failure. */
function check(label: string, ok: boolean, detail: unknown = ''): void {
  if (!ok) failed++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  [worker-decoder-feed] ${label}  ${JSON.stringify(detail)}`);
}

/** What the feed posted to the worker and sent the server, in order. */
interface Feed {
  /** `config` for a decoder configure, `key`/`delta` for a chunk, `request` for `REQUEST_KEYFRAME`. */
  log: string[];
  /**
   * Feeds one frame.
   * @param isKey Whether it is a key frame.
   * @param w Its coded width.
   * @param h Its coded height.
   * @returns What the feed answered: true where the worker took it.
   */
  frame: (isKey: boolean, w?: number, h?: number) => boolean;
  /** @returns The log's entries since the last call. */
  take: () => string[];
  /** @returns How many times the feed started the stream. */
  starts: () => number;
}

const CODEC = 'avc1.42e01f';

/**
 * The feed over a fresh page state: a worker that is up and has configured
 * nothing yet.
 * @param framing The H.264 framing the engine takes.
 * @returns The feed.
 */
function feed(framing: 'annexb' | 'avcc'): Feed {
  const log: string[] = [];
  let taken = 0;
  let starts = 0;
  const context: Record<string, unknown> = vm.createContext({
    workerDecodeFailed: false,
    workerDecoderCodec: null,
    workerDecoderW: 0,
    workerDecoderH: 0,
    preferSoftwareDecode: false,
    videoWorker: {
      postMessage: (message: { type: string; key?: boolean }): void => {
        log.push(message.type === 'decoderConfig' ? 'config' : message.key ? 'key' : 'delta');
      },
    },
    ensureVideoWorker: (): boolean => true,
    activateWorkerSinkDisplay: (): boolean => true,
    h264Framing: (): string => framing,
    logWorkerDecoderConfig: (): void => undefined,
    avcDescription: (): Uint8Array => new Uint8Array([1]),
    annexbToAvcc: (bytes: Uint8Array): Uint8Array => bytes.slice(),
    chunkStamp: (): number => 0,
    requestKeyframe: (): void => { log.push('request'); },
    streamStarted: false,
    startStream: (): void => { starts++; context.streamStarted = true; },
  });
  const compiled: unknown = vm.runInContext(
    `${declaration(readSource(CORE), 'feedWorkerDecoder')}\nfeedWorkerDecoder`, context,
    { filename: 'selkies-ws-core.js' });
  if (typeof compiled !== 'function') throw new Error('feedWorkerDecoder is not a function in the core');
  let id = 0;
  return {
    log,
    frame: (isKey: boolean, w = 1280, h = 720): boolean => {
      id++;
      const handled: unknown = Reflect.apply(compiled, undefined,
        [isKey, new Uint8Array([0, 0, 0, 1, isKey ? 0x65 : 0x41]).buffer, w, h, CODEC, id, isKey ? id : id - 1, NaN]);
      return handled === true;
    },
    take: (): string[] => { const out = log.slice(taken); taken = log.length; return out; },
    starts: (): number => starts,
  };
}

const same = (a: string[], b: string[]): boolean => JSON.stringify(a) === JSON.stringify(b);

for (const framing of ['annexb', 'avcc'] as const) {
  const f = feed(framing);
  let out = f.frame(true);
  let got = f.take();
  check(`${framing}: a key frame configures the decoder and is decoded, with no key frame request`,
        out && same(got, ['config', 'key']), got);
  f.frame(false);
  got = f.take();
  check(`${framing}: the delta frames after it are decoded without a request`, same(got, ['delta']), got);
  out = f.frame(true, 1920, 1080);
  got = f.take();
  check(`${framing}: a key frame at a new size reconfigures, again with no request`,
        out && same(got, ['config', 'key']), got);
  check(`${framing}: the first frame handed on starts the stream, once`, f.starts() === 1, f.starts());
}

{
  const f = feed('annexb');
  const out = f.frame(false);
  const got = f.take();
  check('annexb: a delta frame that configures the decoder asks for a key frame',
        out && same(got, ['config', 'request', 'delta']), got);
  f.frame(true);
  const after = f.take();
  check('and the key frame it asked for is decoded as it comes, with no further request',
        same(after, ['key']), after);
}

{
  const f = feed('avcc');
  const out = f.frame(false);
  const got = f.take();
  check('avcc: a delta frame before any key frame is dropped behind a key frame request',
        out && same(got, ['request']), got);
  check('and starts no stream, since nothing was handed on', f.starts() === 0, f.starts());
  f.frame(true);
  f.frame(false, 1920, 1080);
  const resized = f.take();
  check('avcc: a delta frame at a new size is dropped behind a request too, leaving the decoder as it is',
        same(resized, ['config', 'key', 'request']), resized);
}

console.log(`[worker-decoder-feed] ${failed ? 'FAILED' : 'OK'}`);
process.exit(failed ? 1 : 0);
