/**
 * TypeDoc plugin for the web Developer Reference (loaded by
 * generate-web-docs.mjs).
 *
 * TypeDoc documents exports alone, which fits a library but not the streaming
 * cores: each exports one function and keeps everything else as closures
 * inside it, and the dashboards keep their handlers inside their components.
 * This plugin therefore converts every function that carries a docblock —
 * module-level or nested — into its module's page, so the reference covers
 * what the docblocks cover, as griffe does for the Python modules. A default
 * export is named after its declaration rather than `default`.
 *
 * `_`-prefixed members are dropped from the reference: the web sources mark
 * private state and helpers by that prefix rather than TypeScript's `private`
 * keyword, which is all TypeDoc's excludePrivate understands. Finally the
 * front matter fumadocs needs is written, since typedoc-plugin-markdown emits
 * none.
 */
import ts from 'typescript';
import { Converter, ReflectionKind } from 'typedoc';
import { MarkdownPageEvent } from 'typedoc-plugin-markdown';

const MEMBER_KINDS =
  ReflectionKind.Property |
  ReflectionKind.Method |
  ReflectionKind.Accessor |
  ReflectionKind.Function |
  ReflectionKind.Variable |
  ReflectionKind.Class |
  ReflectionKind.Interface |
  ReflectionKind.TypeAlias;

/** The identifier of a documented function declaration or function-valued `const`, else undefined. */
function documentedFunctionName(node) {
  if (ts.isFunctionDeclaration(node) && node.name) {
    return ts.getJSDocCommentsAndTags(node).length ? node.name : undefined;
  }
  if (
    ts.isVariableDeclaration(node) &&
    ts.isIdentifier(node.name) &&
    node.initializer &&
    (ts.isArrowFunction(node.initializer) || ts.isFunctionExpression(node.initializer))
  ) {
    return ts.getJSDocCommentsAndTags(node).length ? node.name : undefined;
  }
  return undefined;
}

/** Documented functions below `sourceFile` that TypeDoc's export walk does not reach. */
function convertNestedFunctions(context, moduleReflection) {
  const moduleSymbol = context.getSymbolFromReflection(moduleReflection);
  const sourceFile = moduleSymbol?.declarations?.find(ts.isSourceFile);
  if (!sourceFile) return;
  const exported = new Set(context.checker.getExportsOfModule(moduleSymbol).map((s) => s.name));
  const scope = context.withScope(moduleReflection);
  const visit = (node) => {
    const name = documentedFunctionName(node);
    if (name) {
      const topLevel = ts.isFunctionDeclaration(node)
        ? node.parent === sourceFile
        : node.parent.parent.parent === sourceFile;
      if (!(topLevel && exported.has(name.text)) && !name.text.startsWith('_')) {
        const symbol = context.getSymbolAtLocation(name);
        if (symbol) context.converter.convertSymbol(scope, symbol);
      }
    }
    ts.forEachChild(node, visit);
  };
  ts.forEachChild(sourceFile, visit);
}

export function load(app) {
  app.converter.on(Converter.EVENT_CREATE_DECLARATION, (context, reflection) => {
    if (reflection.kindOf(ReflectionKind.Module)) {
      convertNestedFunctions(context, reflection);
    } else if (reflection.name === 'default') {
      const declaration = context.getSymbolFromReflection(reflection)?.declarations?.[0];
      if (declaration?.name && ts.isIdentifier(declaration.name)) reflection.name = declaration.name.text;
    }
  });

  app.converter.on(Converter.EVENT_RESOLVE_BEGIN, (context) => {
    const project = context.project;
    for (const reflection of Object.values(project.reflections)) {
      if (reflection.kindOf(MEMBER_KINDS) && reflection.name.startsWith('_')) {
        project.removeReflection(reflection);
      }
    }
  });

  app.renderer.on(MarkdownPageEvent.END, (page) => {
    page.contents = `---\ntitle: ${JSON.stringify(page.model.name)}\n---\n\n${page.contents ?? ''}`;
  });
}
