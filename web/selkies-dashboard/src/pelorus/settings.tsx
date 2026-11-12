/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The model configurations of the Pelorus chat: the list, and the form one is
 * added or changed with.
 *
 * A configuration is a name, a provider, a model, a key, an effort, and
 * whether the model takes pictures. The provider is picked from the ones the
 * server knows, which carry their own addresses, so the form asks only for
 * what is not public: the endpoint field shows for a local or custom server
 * alone, and the key field not at all for a server that takes none. The
 * server's own configuration is listed and cannot be changed here. A key is typed and
 * sent, never shown: an edit that leaves the field empty keeps the key the
 * server has. The model field is the provider's list (`names.ts`), asked for
 * as soon as the form knows the provider, without a key first and again once
 * one is typed, so the list is there before the person reaches the field.
 * A name left empty is the provider's.
 *
 * A key is sealed by this browser unless the person keeps it on the server
 * (`keys.ts`): sealed, the server holds only what this browser can open, and
 * the configuration runs from this browser alone; kept, the server holds the
 * key, and scripts against the API and every browser can run it. A page that
 * cannot seal (one served over plain HTTP) keeps the key on the server and
 * says why. Moving a key between the two needs it in hand: a sealed key is
 * opened here and sent to be kept, while a kept one, which the server never
 * gives back, has to be typed again to be sealed. Drawn only where the
 * operator has not locked the configuration.
 * @module
 */
import * as React from "react";
import { Pencil, Plus, Star, Trash2 } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { t } from "@/i18n";
import { createModel, deleteModel, updateModel, type ModelConfig, type ModelDraft, type ModelList } from "@/pelorus/api";
import { canSeal, forget, seal, unseal, useHeld } from "@/pelorus/keys";
import { listingKey, useModelNames } from "@/pelorus/names";
import { Choice, ModelPicker } from "@/pelorus/pickers";

interface ModelSettingsProps {
  base: string;
  list: ModelList;
  /** Called after a configuration was saved or deleted, to read the list again. */
  onChanged: () => Promise<void> | void;
  /** Called by the list's close button; the list draws none where it is absent. */
  onClose?: () => void;
}

/** The fields the form edits; the sealed key is the form's own business. */
type Draft = Required<Omit<ModelDraft, "sealed">>;

/** What the form holds: a draft, and the configuration it edits where it is not a new one. */
interface FormState {
  id?: string;
  hasKey: boolean;
  /** The key the configuration holds sealed; empty where the server keeps it or there is none. */
  sealed: string;
  /** Keep the key on the server rather than seal it here. */
  onServer: boolean;
  draft: Draft;
}

const EMPTY: Draft = { name: "", provider: "anthropic", endpoint: "", model: "", api_key: "", effort: "default", vision: false, default: false };

/** A form for a new configuration: its key sealed here wherever this page can seal. */
function newForm(): FormState {
  return { hasKey: false, sealed: "", onServer: !canSeal(), draft: EMPTY };
}

/** The form over an existing configuration. Its endpoint and model stay empty where they are the provider's own. */
function formFor(config: ModelConfig, list: ModelList): FormState {
  const defaults = list.providers.find((p) => p.name === config.provider);
  return {
    id: config.id,
    hasKey: config.has_key,
    sealed: config.sealed,
    onServer: !config.sealed && (config.has_key || !canSeal()),
    draft: {
      name: config.name, provider: config.provider,
      endpoint: config.endpoint === defaults?.endpoint ? "" : config.endpoint,
      model: config.model === defaults?.model ? "" : config.model,
      api_key: "", effort: config.effort, vision: config.vision, default: config.default,
    },
  };
}

/** A labeled field: the label above its control, and a hint under it where one is given. */
function Field({ id, label, hint, children }: { id: string; label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      <Label htmlFor={id} className="text-sm">{label}</Label>
      {children}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

/** A switch with its label beside it, the whole row its target. */
function Toggle({ id, label, checked, onChange }: { id: string; label: string; checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <label htmlFor={id} className="flex cursor-pointer items-center justify-between gap-3 rounded-lg border border-border px-3 py-2.5">
      <span className="text-sm font-medium">{label}</span>
      <Switch id={id} checked={checked} onCheckedChange={onChange} />
    </label>
  );
}

/** The form a configuration is added or changed with. */
function ModelForm({ base, list, form, setForm, onDone, onCancel }: {
  base: string;
  list: ModelList;
  form: FormState;
  setForm: (form: FormState) => void;
  onDone: () => Promise<void>;
  onCancel?: () => void;
}) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const id = React.useId();
  const draft = form.draft;
  const defaults = list.providers.find((p) => p.name === draft.provider);
  const set = (change: Partial<Draft>) => setForm({ ...form, draft: { ...draft, ...change } });
  const endpoint = draft.endpoint || defaults?.endpoint || "";
  const sealable = canSeal();
  const takesKey = defaults?.key !== "none";
  const onServer = form.onServer || !sealable;
  const probe = { id: form.id, provider: draft.provider, endpoint: draft.endpoint, api_key: draft.api_key || undefined };
  const names = useModelNames(base, endpoint ? listingKey(draft.provider, endpoint) : "",
    draft.api_key || !form.sealed ? probe : async () => ({ ...probe, api_key: await unseal(form.sealed) }));

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const body: ModelDraft = { ...draft, name: draft.name.trim() || defaults?.label || draft.provider };
      delete body.api_key;
      const typed = draft.api_key;
      if (takesKey && onServer) {
        if (typed) body.api_key = typed;
        else if (form.sealed) {
          const opened = await unseal(form.sealed);
          if (!opened) throw new Error(t("pelorus.reenterKey"));
          body.api_key = opened;
        }
      } else if (takesKey) {
        if (typed) body.sealed = await seal(typed);
        else if (form.hasKey && !form.sealed) throw new Error(t("pelorus.reenterToSeal"));
      }
      if (form.id) await updateModel(base, form.id, body);
      else await createModel(base, body);
      if (form.sealed && (body.sealed || body.api_key)) await forget(form.sealed);
      await onDone();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="flex flex-col gap-5" onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <div className="grid gap-4 sm:grid-cols-2">
        <Field id={`${id}-provider`} label={t("pelorus.provider")}>
          <Choice id={`${id}-provider`} label={t("pelorus.provider")} value={draft.provider}
            options={list.providers.map((p) => ({ value: p.name, label: p.label }))}
            onChange={(provider) => set({ provider, endpoint: "", model: "" })} />
        </Field>
        <Field id={`${id}-name`} label={t("pelorus.name")}>
          <Input id={`${id}-name`} value={draft.name} placeholder={defaults?.label} onChange={(event) => set({ name: event.target.value })} />
        </Field>
        {(defaults?.local || draft.endpoint !== "") && (
          <div className="sm:col-span-2">
            <Field id={`${id}-endpoint`} label={t("pelorus.endpoint")}>
              <Input id={`${id}-endpoint`} value={draft.endpoint} placeholder={defaults?.endpoint || "https://"} required={!defaults?.endpoint}
                onChange={(event) => set({ endpoint: event.target.value })} />
            </Field>
          </div>
        )}
        {defaults?.key !== "none" && (
          <div className="sm:col-span-2">
            <Field id={`${id}-key`} label={t("pelorus.accessLabel")} hint={form.hasKey ? t("pelorus.keyKeptHint") : undefined}>
              <Input id={`${id}-key`} type="password" autoComplete="off" value={draft.api_key} placeholder={form.hasKey ? t("pelorus.keyKept") : ""}
                required={defaults?.key === "required" && !form.hasKey} onChange={(event) => set({ api_key: event.target.value })} />
            </Field>
          </div>
        )}
        {takesKey && (
          <div className="flex flex-col gap-1.5 sm:col-span-2">
            <label htmlFor={`${id}-server`} className={`flex items-center justify-between gap-3 rounded-lg border border-border px-3 py-2.5 ${sealable ? "cursor-pointer" : "opacity-70"}`}>
              <span className="text-sm font-medium">{t("pelorus.keepOnServer")}</span>
              <Switch id={`${id}-server`} checked={onServer} disabled={!sealable} onCheckedChange={(value) => setForm({ ...form, onServer: value })} />
            </label>
            <p className="text-xs text-muted-foreground">{t(!sealable ? "pelorus.sealUnavailable" : onServer ? "pelorus.keepOnServerOn" : "pelorus.keepOnServerOff")}</p>
          </div>
        )}
        <div className="sm:col-span-2">
          <Field id={`${id}-model`} label={t("pelorus.model")}>
            <ModelPicker id={`${id}-model`} className="w-full" value={draft.model} defaultModel={defaults?.model || undefined} names={names}
              onChange={(model) => set({ model })} />
          </Field>
        </div>
        <Field id={`${id}-effort`} label={t("pelorus.effort")}>
          <Choice id={`${id}-effort`} label={t("pelorus.effort")} value={draft.effort}
            options={list.efforts.map((level) => ({ value: level, label: level }))} onChange={(effort) => set({ effort })} />
        </Field>
        <div className="hidden sm:block" />
        <Toggle id={`${id}-vision`} label={t("pelorus.vision")} checked={draft.vision} onChange={(vision) => set({ vision })} />
        <Toggle id={`${id}-default`} label={t("pelorus.default")} checked={draft.default} onChange={(value) => set({ default: value })} />
      </div>
      {error && <div className="rounded-lg border border-destructive/50 bg-destructive/10 px-3 py-2 text-sm">{error}</div>}
      <div className="flex justify-end gap-2 border-t border-border pt-4">
        {onCancel && <Button type="button" variant="outline" onClick={onCancel}>{t("pelorus.back")}</Button>}
        <Button type="submit" disabled={busy}>{t("pelorus.save")}</Button>
      </div>
    </form>
  );
}

/** Says whether this browser holds what opens a sealed key. */
function SealedBadge({ sealed }: { sealed: string }) {
  const held = useHeld(sealed);
  if (held === undefined) return null;
  return <Badge variant={held ? "secondary" : "destructive"}>{t(held ? "pelorus.sealedHere" : "pelorus.sealedElsewhere")}</Badge>;
}

/** Renders the list of configurations, or the form while one is being added or changed. */
export function ModelSettings({ base, list, onChanged, onClose }: ModelSettingsProps) {
  const editable = list.models.some((m) => m.editable);
  const [form, setForm] = React.useState<FormState | null>(editable ? null : newForm);
  const [error, setError] = React.useState<string | null>(null);

  if (form) {
    return (
      <ModelForm key={form.id ?? "new"} base={base} list={list} form={form} setForm={setForm}
        onDone={async () => { await onChanged(); setForm(null); }}
        onCancel={editable || list.models.length > 0 ? () => setForm(null) : undefined} />
    );
  }

  const remove = async (config: ModelConfig) => {
    setError(null);
    try {
      await deleteModel(base, config.id);
      if (config.sealed) await forget(config.sealed);
      await onChanged();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-col gap-2">
        {list.models.map((config) => (
          <div key={config.id} className="flex items-center gap-3 rounded-lg border border-border bg-card px-3 py-2.5">
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate font-medium">{config.name}</span>
                {config.default && <Star className="size-3.5 shrink-0 fill-current text-amber-500" aria-label={t("pelorus.default")} />}
                {!config.usable && <Badge variant="destructive">{t("pelorus.incomplete")}</Badge>}
                {!config.editable && <Badge variant="secondary">{t("pelorus.fromServer")}</Badge>}
                {config.sealed && <SealedBadge sealed={config.sealed} />}
              </div>
              <div className="flex min-w-0 items-center gap-2 text-xs text-muted-foreground">
                <span className="shrink-0">{list.providers.find((p) => p.name === config.provider)?.label ?? config.provider}</span>
                <span aria-hidden>·</span>
                <span className="truncate">{config.model}</span>
              </div>
            </div>
            {config.editable && (
              <div className="flex shrink-0 gap-1">
                <Button variant="ghost" size="icon-sm" title={t("pelorus.edit")} aria-label={t("pelorus.edit")} onClick={() => setForm(formFor(config, list))}><Pencil /></Button>
                <Button variant="ghost" size="icon-sm" className="hover:text-destructive" title={t("pelorus.delete")} aria-label={t("pelorus.delete")}
                  onClick={() => void remove(config)}><Trash2 /></Button>
              </div>
            )}
          </div>
        ))}
      </div>
      {error && <div className="rounded-lg border border-destructive/50 bg-destructive/10 px-3 py-2 text-sm">{error}</div>}
      <div className="flex justify-end gap-2 border-t border-border pt-4">
        {onClose && <Button variant="outline" onClick={onClose}>{t("pelorus.back")}</Button>}
        <Button onClick={() => setForm(newForm())}><Plus />{t("pelorus.add")}</Button>
      </div>
    </div>
  );
}
