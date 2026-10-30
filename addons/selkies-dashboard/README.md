# Selkies Dashboard

The web interface served over the streaming core: React and TypeScript, built with Vite, drawn with Tailwind and the shadcn/ui primitives vendored under `src/components/ui`. It talks to `selkies-core` through `window.postMessage` alone, so it is also the example a dashboard of your own starts from: the core keeps streaming whatever paints the controls.

## Layout

```
src/
  main.tsx          bootstrap: probes /api/status, starts the runtime-served core, mounts App
  App.tsx           the providers, the layout, and what every layout shares (dialogs, notices, floating stats)
  state/            dashboard.tsx (the shared state and the core's echoes), settings.ts (the stream settings' rules)
  panels/           one component per panel, drawn wherever a layout puts it
  layouts/          index.ts (the registry), sidebar.tsx (the default), toolbar.tsx (wish)
  i18n.ts           the translator over translations.ts, one dictionary per locale
  utils.ts          storage keys, the core's cached broadcasts, the visibility flags the server's settings derive
```

A layout is a component that reads `useDashboard()` and `useSettings()` and decides where each panel is painted. The sidebar lays them out as sections of a drawer; the toolbar hangs them from a floating bar as menus, tabs, and floating panels. Adding a layout is one entry in `layouts/index.ts`, its value in the server's `ui_layout` enum, and its name in the dictionary.

## Core interaction

The core posts status and events as `message` events on `window`, each a `{type, ...}` object from the page's own origin: `serverSettings` (the settings payload every visibility flag derives from), `pipelineStatusUpdate`, `clipboardContentUpdate`, `gamepadButtonUpdate`, `fileUpload`, `printDocument`, `transportAdvice`, and the rest the state modules list in their docblocks. The dashboard posts back `settings` (debounced key/value batches the core forwards to the server), `pipelineControl`, `gamepadControl`, `setManualResolution`, `clipboardUpdateFromUI`, `requestFullscreen`, and the others named beside the control that sends them. Stream statistics come from `window.stream_info`, `window.stream_client`, and `window.stream_stats`, read whenever the core announces a change.

Every setting lives in `localStorage` under `<storageAppName>_<key>`, the per-display keys gaining a `_display2` suffix on the second display, and the cores read the same keys. The theme is stored unprefixed under `theme`, which the server's file index mirrors.

## Building

```sh
npm ci
npm run build      # prebuild copies the core from ../selkies-web-core/dist into public/
npm run dev        # the same against Vite's dev server
npm run lint
npm run typecheck
```

The core has to be built first (`npm --prefix ../selkies-web-core run build`, or `scripts/ci/build-web.sh` from the repository root, which builds both and bundles the result into the wheel). `npm run accent` rewrites the theme's accent in `src/index.css` from the logo's colors after the shadcn CLI regenerates it.
