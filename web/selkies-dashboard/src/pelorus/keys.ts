/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * API keys sealed by this browser: the server keeps what this module
 * encrypts, and only this browser can open it.
 *
 * Sealing a key makes an AES-GCM key of its own in Web Crypto, marked
 * non-extractable so no script can read it out, and keeps it in this
 * origin's IndexedDB under a random id. What the server is given is
 * `v1.<id>.<iv>.<ciphertext>`, the last two in base64: the id says which of
 * this browser's keys opens it, so one browser can hold several. A run or a
 * model listing opens it here and sends the API key with that request alone
 * (`configs.rs` on the server).
 *
 * Web Crypto is offered only to a secure context (HTTPS, or the loopback
 * address), so a page served over plain HTTP cannot seal and keeps its keys
 * on the server (`canSeal`). Clearing this site's data loses the keys that
 * open the sealed ones, and the configurations they belong to have to be
 * given their key again.
 * @module
 */

import * as React from "react";

const DB_NAME = "pelorus-keys";
const STORE = "keys";
const PREFIX = "v1";

/** Whether this page can seal keys: a secure context with Web Crypto and IndexedDB. */
export function canSeal(): boolean {
  return typeof window !== "undefined" && window.isSecureContext && !!window.crypto?.subtle && typeof indexedDB !== "undefined";
}

/** Opens the database of this browser's sealing keys, making it on first use. */
function database(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("IndexedDB refused"));
  });
}

/**
 * Runs one request against the store and closes the database after.
 * @param mode The transaction's mode.
 * @param work Makes the request.
 * @returns What the request answered.
 */
async function withStore<T>(mode: IDBTransactionMode, work: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const db = await database();
  try {
    return await new Promise<T>((resolve, reject) => {
      const request = work(db.transaction(STORE, mode).objectStore(STORE));
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error ?? new Error("IndexedDB refused"));
    });
  } finally {
    db.close();
  }
}

/** Bytes as base64. */
function toBase64(bytes: Uint8Array): string {
  let text = "";
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text);
}

/** Base64 as bytes. */
function fromBase64(text: string): Uint8Array<ArrayBuffer> {
  const raw = atob(text);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
  return bytes;
}

/**
 * Seals an API key with a new key this browser keeps.
 * @param apiKey The key as typed.
 * @returns What the server keeps in its place.
 * @throws Where this page cannot seal (`canSeal`) or the browser refuses to store the key.
 */
export async function seal(apiKey: string): Promise<string> {
  if (!canSeal()) throw new Error("This page cannot seal keys.");
  const key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
  const id = toBase64(crypto.getRandomValues(new Uint8Array(12))).replace(/\+/g, "-").replace(/\//g, "_");
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, new TextEncoder().encode(apiKey)));
  await withStore("readwrite", (store) => store.put(key, id));
  return [PREFIX, id, toBase64(iv), toBase64(data)].join(".");
}

/** The id of the key that opens a sealed one; `null` where it does not read. */
function keyId(sealed: string): string | null {
  const parts = sealed.split(".");
  return parts.length === 4 && parts[0] === PREFIX ? parts[1] : null;
}

/**
 * Opens a key this browser sealed.
 * @param sealed What the server keeps.
 * @returns The API key, or `undefined` where this browser does not hold what opens it.
 */
export async function unseal(sealed: string): Promise<string | undefined> {
  const id = keyId(sealed);
  if (!id || !canSeal()) return undefined;
  try {
    const key = await withStore<CryptoKey | undefined>("readonly", (store) => store.get(id) as IDBRequest<CryptoKey | undefined>);
    if (!key) return undefined;
    const [, , iv, data] = sealed.split(".");
    const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: fromBase64(iv) }, key, fromBase64(data));
    return new TextDecoder().decode(plain);
  } catch {
    return undefined;
  }
}

/**
 * Forgets the key that opens a sealed one, once its configuration is gone.
 * @param sealed What the server kept.
 */
export async function forget(sealed: string): Promise<void> {
  const id = keyId(sealed);
  if (!id || !canSeal()) return;
  await withStore("readwrite", (store) => store.delete(id)).catch(() => undefined);
}

/**
 * Whether this browser holds what opens a sealed key.
 * @param sealed What the server keeps; empty for a key it keeps itself.
 * @returns `true` or `false` once known, `undefined` while it is being found out or where nothing is sealed.
 */
export function useHeld(sealed: string): boolean | undefined {
  const [held, setHeld] = React.useState<{ sealed: string; held: boolean } | null>(null);
  React.useEffect(() => {
    if (!sealed) return;
    let live = true;
    void unseal(sealed).then((key) => { if (live) setHeld({ sealed, held: key !== undefined }); });
    return () => { live = false; };
  }, [sealed]);
  return sealed && held?.sealed === sealed ? held.held : undefined;
}
