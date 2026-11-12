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
 * server has. The model field offers the provider's own list once it is
 * fetched with what the form holds, a key typed but not yet saved included.
 * Drawn only where the operator has not locked the configuration.
 * @module
 */
import * as React from "react";
import { Pencil, Star, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { t } from "@/i18n";
import { createModel, deleteModel, modelNames, updateModel, type ModelConfig, type ModelDraft, type ModelList } from "@/pelorus/api";

/** The classes of a native select drawn like the kit's inputs. */
export const selectClass = "h-7 max-w-full min-w-0 rounded-md border border-input bg-background px-1.5 text-xs text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/50 disabled:opacity-50";

interface ModelSettingsProps {
  base: string;
  list: ModelList;
  /** Called after a configuration was saved or deleted, to read the list again. */
  onChanged: () => Promise<void> | void;
  onClose: () => void;
}

/** What the form holds: a draft, and the configuration it edits where it is not a new one. */
interface FormState {
  id?: string;
  hasKey: boolean;
  draft: Required<ModelDraft>;
}

const EMPTY: Required<ModelDraft> = { name: "", provider: "anthropic", endpoint: "", model: "", api_key: "", effort: "default", vision: false, default: false };

/** The form over an existing configuration. Its endpoint and model stay empty where they are the provider's own. */
function formFor(config: ModelConfig, list: ModelList): FormState {
  const defaults = list.providers.find((p) => p.name === config.provider);
  return {
    id: config.id,
    hasKey: config.has_key,
    draft: {
      name: config.name, provider: config.provider,
      endpoint: config.endpoint === defaults?.endpoint ? "" : config.endpoint,
      model: config.model === defaults?.model ? "" : config.model,
      api_key: "", effort: config.effort, vision: config.vision, default: config.default,
    },
  };
}

/** Renders the list of configurations, or the form while one is being added or changed. */
export function ModelSettings({ base, list, onChanged, onClose }: ModelSettingsProps) {
  const [form, setForm] = React.useState<FormState | null>(list.models.some((m) => m.editable) ? null : { hasKey: false, draft: EMPTY });
  const [names, setNames] = React.useState<string[]>([]);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const listId = React.useId();

  const guarded = async (work: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };

  if (!form) {
    return (
      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto rounded-lg border border-border bg-background/60 p-2 text-sm">
        {list.models.map((config) => (
          <div key={config.id} className="flex items-center gap-1 rounded-md border border-border px-2 py-1">
            <div className="min-w-0 flex-1">
              <div className="truncate font-medium">{config.name}{config.default && <Star className="ml-1 inline size-3 fill-current" aria-label={t("pelorus.default")} />}</div>
              <div className="truncate text-xs text-muted-foreground">{list.providers.find((p) => p.name === config.provider)?.label ?? config.provider} · {config.model} · {config.effort}{!config.usable && ` · ${t("pelorus.incomplete")}`}</div>
            </div>
            {config.editable ? (
              <>
                <Button variant="ghost" size="xs" title={t("pelorus.edit")} aria-label={t("pelorus.edit")} onClick={() => { setNames([]); setForm(formFor(config, list)); }}><Pencil /></Button>
                <Button variant="ghost" size="xs" title={t("pelorus.delete")} aria-label={t("pelorus.delete")} disabled={busy}
                  onClick={() => void guarded(async () => { await deleteModel(base, config.id); await onChanged(); })}><Trash2 /></Button>
              </>
            ) : <span className="text-xs text-muted-foreground">{t("pelorus.fromServer")}</span>}
          </div>
        ))}
        {error && <div className="text-xs text-destructive">{error}</div>}
        <div className="mt-auto flex justify-end gap-1">
          <Button variant="outline" size="sm" onClick={onClose}>{t("pelorus.back")}</Button>
          <Button size="sm" onClick={() => { setNames([]); setForm({ hasKey: false, draft: EMPTY }); }}>{t("pelorus.add")}</Button>
        </div>
      </div>
    );
  }

  const draft = form.draft;
  const defaults = list.providers.find((p) => p.name === draft.provider);
  const set = (change: Partial<Required<ModelDraft>>) => setForm({ ...form, draft: { ...draft, ...change } });
  const save = () => guarded(async () => {
    const body: ModelDraft = { ...draft };
    if (!draft.api_key) delete body.api_key;
    if (form.id) await updateModel(base, form.id, body);
    else await createModel(base, body);
    await onChanged();
    setForm(null);
  });
  const fetchNames = () => guarded(async () => {
    setNames(await modelNames(base, { id: form.id, provider: draft.provider, endpoint: draft.endpoint, api_key: draft.api_key || undefined }));
  });
  const field = "grid grid-cols-[6.5rem_1fr] items-center gap-2";

  return (
    <form className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto rounded-lg border border-border bg-background/60 p-2 text-sm"
      onSubmit={(event) => { event.preventDefault(); void save(); }}>
      <div className={field}>
        <Label htmlFor={`${listId}-name`}>{t("pelorus.name")}</Label>
        <Input id={`${listId}-name`} value={draft.name} onChange={(event) => set({ name: event.target.value })} required />
      </div>
      <div className={field}>
        <Label htmlFor={`${listId}-provider`}>{t("pelorus.provider")}</Label>
        <select id={`${listId}-provider`} className={`${selectClass} h-8`} value={draft.provider} onChange={(event) => { setNames([]); set({ provider: event.target.value, endpoint: "", model: "" }); }}>
          {list.providers.map((p) => <option key={p.name} value={p.name}>{p.label}</option>)}
        </select>
      </div>
      {(defaults?.local || draft.endpoint !== "") && (
        <div className={field}>
          <Label htmlFor={`${listId}-endpoint`}>{t("pelorus.endpoint")}</Label>
          <Input id={`${listId}-endpoint`} value={draft.endpoint} placeholder={defaults?.endpoint || "https://"} required={!defaults?.endpoint}
            onChange={(event) => set({ endpoint: event.target.value })} />
        </div>
      )}
      {defaults?.key !== "none" && (
        <div className={field}>
          <Label htmlFor={`${listId}-key`}>{t("pelorus.apiKey")}</Label>
          <Input id={`${listId}-key`} type="password" autoComplete="off" value={draft.api_key} placeholder={form.hasKey ? t("pelorus.keyKept") : ""}
            required={defaults?.key === "required" && !form.hasKey} onChange={(event) => set({ api_key: event.target.value })} />
        </div>
      )}
      <div className={field}>
        <Label htmlFor={`${listId}-model`}>{t("pelorus.model")}</Label>
        <div className="flex min-w-0 gap-1">
          <Input id={`${listId}-model`} className="min-w-0 flex-1" list={`${listId}-names`} value={draft.model} placeholder={defaults?.model} onChange={(event) => set({ model: event.target.value })} />
          <datalist id={`${listId}-names`}>{names.map((name) => <option key={name} value={name} />)}</datalist>
          <Button type="button" variant="outline" size="sm" disabled={busy} onClick={() => void fetchNames()}>{t("pelorus.fetchModels")}</Button>
        </div>
      </div>
      <div className={field}>
        <Label htmlFor={`${listId}-effort`}>{t("pelorus.effort")}</Label>
        <select id={`${listId}-effort`} className={`${selectClass} h-8`} value={draft.effort} onChange={(event) => set({ effort: event.target.value })}>
          {list.efforts.map((level) => <option key={level} value={level}>{level}</option>)}
        </select>
      </div>
      <div className={field}>
        <Label htmlFor={`${listId}-vision`}>{t("pelorus.vision")}</Label>
        <Switch id={`${listId}-vision`} checked={draft.vision} onCheckedChange={(vision) => set({ vision })} />
      </div>
      <div className={field}>
        <Label htmlFor={`${listId}-default`}>{t("pelorus.default")}</Label>
        <Switch id={`${listId}-default`} checked={draft.default} onCheckedChange={(value) => set({ default: value })} />
      </div>
      {names.length > 0 && <div className="text-xs text-muted-foreground">{t("pelorus.modelsFound", { count: names.length })}</div>}
      {error && <div className="text-xs text-destructive">{error}</div>}
      <div className="mt-auto flex justify-end gap-1">
        <Button type="button" variant="outline" size="sm" onClick={() => (list.models.some((m) => m.editable) ? setForm(null) : onClose())}>{t("pelorus.back")}</Button>
        <Button type="submit" size="sm" disabled={busy}>{t("pelorus.save")}</Button>
      </div>
    </form>
  );
}
