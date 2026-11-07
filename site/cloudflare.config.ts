import { defineConfig } from "cf/config";

export default defineConfig({
  worker: {
    name: "acropolis-docs",
    compatibilityDate: "2026-11-04",
    assets: {
      htmlHandling: "auto-trailing-slash",
      notFoundHandling: "404-page",
    },
    domains: ["acropolis.olimpia.dev"],
    workersDev: false,
    previewUrls: false,
  },
});
