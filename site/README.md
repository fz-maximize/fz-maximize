# fz-maximize website

The project site, published at https://fz-maximize.github.io/fz-maximize/ by
`.github/workflows/pages.yml` whenever `site/` changes on `main`. Built with
[Astro Starlight](https://starlight.astro.build).

- Pages: `src/content/docs/*.mdx` (Markdown + a few Starlight components). The sidebar is
  in `astro.config.mjs`.
- Internal links must include the base path: `/fz-maximize/settings/`.
- Images: `src/assets/` (optimized at build time); `public/` is copied as is (logo, the
  front-page illustration `hero.svg` and its still version `hero-static.svg`).

Preview locally (needs Node.js LTS):

```powershell
task site        # dev server with live reload at http://localhost:4321/fz-maximize/
task site:build  # production build into site/dist, as CI does
```
