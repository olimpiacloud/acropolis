import { defineConfig } from "blume";

export default defineConfig({
  title: "Acropolis",
  description:
    "Acropolis by Olimpia: an experimental, daemonless builder that turns an app's source code into an OCI image with no Dockerfile. A single Rust binary.",
  logo: {
    image: "/logo.svg",
    text: "Acropolis",
    href: "/",
  },
  github: {
    owner: "olimpiacloud",
    repo: "acropolis",
  },
  banner: {
    content: "Acropolis is experimental: interfaces, output and defaults can change between releases.",
    link: { text: "What that means", href: "/introduction#status" },
  },
  theme: {
    accent: { light: "#9c4a2b", dark: "#d0784f" },
    background: { light: "#efe9dd", dark: "#1b1714" },
    radius: "sm",
    mode: "system",
    fonts: {
      display: { name: "Marcellus", weights: [400], fallback: "serif" },
      body: { name: "Commissioner", weights: ["100..900"] },
      mono: "jetbrains-mono",
    },
  },
  navigation: {
    cta: { href: "https://github.com/olimpiacloud/acropolis", label: "GitHub" },
    actions: [
      { href: "https://olimpia.dev", label: "olimpia.dev" },
      { href: "https://agora.olimpia.dev", label: "Agora" },
    ],
  },
  markdown: {
    imageZoom: true,
    code: {
      theme: {
        light: "github-light",
        dark: "vesper",
      },
    },
  },
  footer: {
    links: [
      { label: "Olimpia", href: "https://olimpia.dev" },
      { label: "Olimpia docs", href: "https://docs.olimpia.dev" },
      { label: "Agora", href: "https://agora.olimpia.dev" },
      { label: "GitHub", href: "https://github.com/olimpiacloud/acropolis" },
    ],
  },
  agents: {
    llmsTxt: {
      details:
        "Acropolis is an experimental app-to-OCI-image builder by Olimpia (https://olimpia.dev): a single Rust binary (`acropolis`) for Linux x86_64 and arm64 that detects the language and framework, downloads verified toolchains and dependencies, compiles, and assembles the image with no Dockerfile and no daemon. `acropolis build ./app -t registry/app:tag` pushes to a registry, `--oci image.tar` writes a tar for `docker load`, `acropolis plan ./app` prints the build plan. Source: https://github.com/olimpiacloud/acropolis (Apache-2.0 or MIT).",
    },
  },
  lastModified: "git",
  deployment: {
    site: "https://acropolis.olimpia.dev",
  },
});
