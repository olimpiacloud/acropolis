export type Product = "olimpia" | "agora" | "acropolis";

const sites: Record<Product, { name: string; url: string; repo: string; tag?: string }> = {
  olimpia: { name: "Olimpia", url: "https://docs.olimpia.dev", repo: "https://github.com/olimpiacloud" },
  agora: { name: "Agora", url: "https://agora.olimpia.dev", repo: "https://github.com/olimpiacloud/agora" },
  acropolis: {
    name: "Acropolis",
    url: "https://acropolis.olimpia.dev",
    repo: "https://github.com/olimpiacloud/acropolis",
    tag: "Experimental",
  },
};

const sections: Record<Product, { agents: string; humans: string }> = {
  olimpia: { agents: "/agentes", humans: "/introduccion" },
  agora: { agents: "/agents", humans: "/introduction" },
  acropolis: { agents: "/agents", humans: "/introduction" },
};

const ctas: Record<Product, { href: string; label: string | Record<string, string> }> = {
  olimpia: { href: "https://olimpia.dev/dashboard", label: { en: "Open the console", es: "Abrir la consola" } },
  agora: { href: "/install", label: { en: "Install", es: "Instalar" } },
  acropolis: { href: "https://github.com/olimpiacloud/acropolis", label: "GitHub" },
};

const defaultLocales: Record<Product, "en" | "es"> = { olimpia: "es", agora: "en", acropolis: "en" };

export const tagline = {
  en: "What is built on stone, lasts.",
  es: "Lo que se construye sobre piedra, dura.",
};

export function family(product: Product) {
  const site = sites[product];
  const section = sections[product];
  return {
    i18n: {
      defaultLocale: defaultLocales[product],
      locales: [
        { code: "en", label: "English", style: "Clear, direct, global English" },
        { code: "es", label: "Español", style: "Español rioplatense, voseo, tono directo" },
      ],
      routeByBrowserLanguage: true,
    },
    theme: {
      accent: { light: "#1d63d8", dark: "#6aa8ff" },
      background: { light: "#ffffff", dark: "#070b16" },
      radius: "sm" as const,
      mode: "system" as const,
      fonts: {
        display: { name: "Redaction 10", provider: "fontsource", weights: [400, 700], fallback: "serif" },
        body: { name: "Commissioner", weights: ["100..900"] },
        mono: "jetbrains-mono",
      },
    },
    navigation: {
      tabs: [
        { label: { en: "For agents", es: "Para agentes" }, path: section.agents, icon: "bot" },
        { label: { en: "For humans", es: "Para humanos" }, path: "/", href: section.humans, icon: "book-open" },
      ],
      selectors: [
        {
          kind: "product" as const,
          label: "Olimpia",
          items: (Object.keys(sites) as Product[]).map((key) => ({
            label: sites[key].name,
            path: key === product ? "/" : sites[key].url,
            ...(sites[key].tag ? { tag: sites[key].tag } : {}),
          })),
        },
      ],
      cta: ctas[product],
      actions: [{ href: "https://olimpia.dev", label: "olimpia.dev" }],
      repo: false,
    },
    ...(product === "acropolis"
      ? {
          banner: {
            content: {
              en: "Acropolis is experimental: interfaces, output and defaults can change between releases.",
              es: "Acropolis es experimental: la interfaz, la salida y los valores por defecto pueden cambiar entre versiones.",
            },
            link: { text: { en: "What that means", es: "Qué significa" }, href: "/introduction#status" },
          },
        }
      : {}),
    markdown: {
      imageZoom: true,
      code: { theme: { light: "github-light", dark: "vesper" } },
    },
    footer: {
      copyright: tagline,
      links: [
        { label: "Olimpia", href: "https://olimpia.dev" },
        { label: "Olimpia Docs", href: sites.olimpia.url },
        { label: "Agora", href: sites.agora.url },
        { label: { en: "Acropolis (experimental)", es: "Acropolis (experimental)" }, href: sites.acropolis.url },
        { label: "GitHub", href: site.repo },
        { label: "hola@olimpia.dev", href: "mailto:hola@olimpia.dev" },
      ],
    },
    seo: {
      og: { logo: "/logo.svg", palette: { background: "#ffffff", foreground: "#0e1726", accent: "#1d63d8" } },
    },
    lastModified: "git" as const,
    deployment: { site: site.url },
  };
}
