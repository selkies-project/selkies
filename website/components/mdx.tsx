import defaultMdxComponents from 'fumadocs-ui/mdx';
import * as PythonComponents from 'fumadocs-python/components';
import type { MDXComponents } from 'mdx/types';
import type { ReactNode } from 'react';

/*
 * fumadocs-python's own PySourceCode hands Base UI's client-side Collapsible a
 * function-valued className from a Server Component, which React rejects when
 * prerendering the static export. A plain <details> needs no client boundary.
 */
function PySourceCode({ children }: { children: ReactNode }) {
  return (
    <details className="my-6 rounded-lg border bg-fd-secondary">
      <summary className="cursor-pointer select-none px-3 py-2 text-sm font-medium text-fd-secondary-foreground">
        Source Code
      </summary>
      <div className="prose-no-margin px-3 pb-3">{children}</div>
    </details>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    // PyFunction/PyAttribute/... and Tabs, used by the generated pages under
    // docs/reference (see scripts/generate-python-docs.mjs).
    ...PythonComponents,
    PySourceCode,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
