# Selkies documentation site

The [Fumadocs](https://fumadocs.dev) site published at
<https://selkies-project.github.io/selkies>. It renders the Markdown in
[`../docs`](../docs) and nothing else; the pages stay there so they remain
readable, reviewable, and editable straight from GitHub.

```bash
npm install
npm run dev          # http://localhost:3000
npm run build        # static site in out/
npm run check-links  # every link and anchor in out/ must resolve
```

Everything is prerendered: GitHub Pages serves files, so the export carries its
own search index and there is no server at runtime. Each page is written both
as `page.html` and as `page/index.html`, so `/page` and `/page/` both resolve
without a redirect; the slashless form is the canonical one.

`NEXT_PUBLIC_BASE_PATH` is the path the site is served from, `/selkies` in the
`Docs` workflow and empty locally.

Writing a page is covered in
[Development and Contributions](../docs/development.md#documentation).
