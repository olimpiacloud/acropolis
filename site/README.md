# Acropolis docs

Source of [acropolis.olimpia.dev](https://acropolis.olimpia.dev), built with [Blume](https://useblume.dev). Content is in `docs/` (MDX, ordered by `docs/meta.ts`); `blume.config.ts`, `theme.css` and `components/Logo.astro` hold the configuration, the Olimpia palette and the header with the Experimental badge.

```bash
bun install
bun run dev        # http://localhost:4321
bun run build      # static site in dist/
bun run validate   # broken links
```

## Deploy

A static-assets Cloudflare Worker named `acropolis-docs` serves `dist/` on the custom domain `acropolis.olimpia.dev` (`cloudflare.config.ts`, `wrangler.config.ts`). With the `cf` CLI logged in to the Olimpia account:

```bash
bun run deploy     # blume build && cf deploy
```
