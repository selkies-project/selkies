/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The models each provider offers, as last listed, shared by every picker on
 * the page and kept in this browser between visits.
 *
 * A listing is keyed by provider and the endpoint it was asked at, so the
 * page's model picker, the settings form, and a configuration being added all
 * read the same answer, and a configuration on the same server finds the list
 * another one fetched. A picker with nothing listed for its key asks once on
 * its own, without a key where it has none to give: a local server and some
 * hosted ones (OpenRouter) list their models to anyone, and one that refuses
 * costs nothing but the failed request. A typed key asks again, since it can
 * change what the account is offered, and so does the refresh button. A
 * picker over a sealed configuration hands a function that opens its key
 * (`keys.ts`) as the request is made.
 * Requests in flight are shared, so pickers drawn together ask once.
 * @module
 */
import * as React from "react";
import { modelNames, type ModelDraft } from "@/pelorus/api";

/** One provider's models as last listed. */
export interface Listing {
  names: string[];
  /** When it was listed, in milliseconds since the epoch. */
  at: number;
}

/** What a picker asks with: a saved configuration by `id`, any field it has not saved in place of its own. */
export type Probe = Pick<ModelDraft, "provider" | "endpoint" | "api_key"> & { id?: string };

/** What a picker draws. */
export interface NamesState {
  listing?: Listing;
  loading: boolean;
  /** Why the last request failed, until one succeeds. */
  error?: string;
}

const STORAGE_KEY = "pelorus.modelNames";
const listings = new Map<string, Listing>();
const errors = new Map<string, string>();
const pending = new Map<string, Promise<void>>();
const listeners = new Set<() => void>();
const asked = new Set<string>();

try {
  const stored: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
  if (stored && typeof stored === "object") {
    for (const [key, value] of Object.entries(stored as Record<string, Listing>)) {
      if (Array.isArray(value?.names)) listings.set(key, { names: value.names.filter((n) => typeof n === "string"), at: Number(value.at) || 0 });
    }
  }
} catch { /* unreadable or refused: the page lists afresh */ }

/** Tells every picker that something changed. */
function changed(): void {
  listeners.forEach((listener) => listener());
}

/** Keeps the listings for the next visit. */
function persist(): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(Object.fromEntries(listings)));
  } catch { /* storage refused: the listings last for the page */ }
}

/**
 * The key a listing is kept under.
 * @param provider The provider's id.
 * @param endpoint The endpoint the request goes to; empty for the provider's own.
 * @returns The key.
 */
export function listingKey(provider: string, endpoint: string): string {
  return `${provider}|${endpoint.trim().replace(/\/+$/, "")}`;
}

/**
 * Lists a provider's models and keeps the answer.
 * @param base The API's address.
 * @param key The key the answer is kept under.
 * @param probe What the request is made with, or a function that makes it.
 */
export function refreshNames(base: string, key: string, probe: Probe | (() => Promise<Probe>)): Promise<void> {
  const inFlight = pending.get(key);
  if (inFlight) return inFlight;
  asked.add(key);
  const request = (typeof probe === "function" ? probe() : Promise.resolve(probe)).then((resolved) => modelNames(base, resolved)).then(
    (names) => {
      listings.set(key, { names, at: Date.now() });
      errors.delete(key);
      persist();
    },
    (failure: unknown) => {
      errors.set(key, failure instanceof Error ? failure.message : String(failure));
    },
  ).finally(() => {
    pending.delete(key);
    changed();
  });
  pending.set(key, request);
  changed();
  return request;
}

/** Subscribes to every change of the listings. */
function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/**
 * The models a provider offers, listed on first use and on demand.
 * @param base The API's address.
 * @param key The listing's key (`listingKey`); empty where there is nothing to ask yet.
 * @param probe What a request is made with, or a function that makes it.
 * @returns What is listed, whether a request is running, the last failure, and a refresh.
 */
export function useModelNames(base: string, key: string, probe: Probe | (() => Promise<Probe>)): NamesState & { refresh: () => void } {
  const read = React.useCallback(
    () => `${listings.get(key)?.at ?? 0}|${pending.has(key)}|${errors.get(key) ?? ""}`,
    [key],
  );
  React.useSyncExternalStore(subscribe, read);
  const probeRef = React.useRef(probe);
  React.useEffect(() => { probeRef.current = probe; });

  React.useEffect(() => {
    if (key && !listings.has(key) && !asked.has(key)) void refreshNames(base, key, probeRef.current);
  }, [base, key]);

  // A typed key asks again once typing settles: it can change what the account is offered.
  const typedKey = typeof probe === "function" ? "" : probe.api_key ?? "";
  React.useEffect(() => {
    if (!key || !typedKey) return;
    const timer = window.setTimeout(() => void refreshNames(base, key, probeRef.current), 700);
    return () => window.clearTimeout(timer);
  }, [base, key, typedKey]);

  const refresh = React.useCallback(() => {
    if (key) void refreshNames(base, key, probeRef.current);
  }, [base, key]);
  return { listing: listings.get(key), loading: pending.has(key), error: errors.get(key), refresh };
}
