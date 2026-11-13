export type Locale = "en" | "es";

const clients = (planLabel: string, buildLabel: string) => [
  { label: planLabel, icon: "list-checks", command: "acropolis plan ./app --json" },
  { label: buildLabel, icon: "package", command: "acropolis build ./app --oci image.tar --events json" },
  { label: "Claude Code", logo: "/agents/claude.svg", command: "claude mcp add --transport http olimpia https://api.olimpia.dev/mcp" },
  { label: "Codex", logo: "/agents/codex.svg", command: "codex mcp add olimpia --url https://api.olimpia.dev/mcp" },
];

const machine = (markdown: string, page: string) => [
  { href: "/llms.txt", label: "llms.txt" },
  { href: "/llms-full.txt", label: "llms-full.txt" },
  { href: "/skill.md", label: "skill.md" },
  { href: page, label: markdown },
];

export const home = {
  en: {
    title: "Acropolis by Olimpia: source code in, OCI image out",
    description:
      "Acropolis by Olimpia: an experimental, daemonless builder that turns an app's source code into an OCI image with no Dockerfile. A single Rust binary.",
    og: { image: "/arte/og.png", alt: "Acropolis by Olimpia: an acropolis built up in layers facing the bay of Buenos Aires" },
    eyebrow: "Documentation · Experimental",
    heading: "Source code in. A small image out.",
    lede: "Acropolis detects your app's language and framework, fetches verified toolchains and builds an OCI image with no Dockerfile and no daemon. These docs have two doors: one for your agent and one for you.",
    doors: { agents: "Let your agent do it", humans: "Do it yourself" },
    agents: {
      intro:
        "Acropolis has no MCP server of its own: your agent runs the CLI and reads its JSON. To build and deploy on Olimpia, connect Olimpia's MCP server and choose Acropolis as the builder.",
      clients: clients("Plan", "Build"),
      copy: "Copy the command for",
      machineIntro: "These docs also read without HTML: ",
      machine: machine("Markdown for every page", "/introduction.md"),
      tasks: [
        { href: "/agents/connect", icon: "plug", title: "Connect", body: "Give your agent the docs, the binary and machine-readable output." },
        { href: "/agents/tools", icon: "wrench", title: "Tools", body: "plan, build, inspect and cache as agent tools, with their JSON and exit codes." },
        { href: "/agents/workflows", icon: "route", title: "Workflows", body: "Check that an app builds, react to a failed build, fix detection and build on Olimpia." },
      ],
      cta: { href: "/agents", label: "Guide for agents" },
    },
    humans: {
      guides: [
        { href: "/install", icon: "download", title: "Install", body: "Release binaries for Linux x86_64 and arm64, with checksums and provenance." },
        { href: "/how-it-works", icon: "layers", title: "How it works", body: "Detection, plans, verified toolchains and how the image is assembled." },
        { href: "/usage", icon: "terminal", title: "Usage", body: "Build to a registry or to a tar, print the plan and pass build variables." },
        { href: "/compatibility", icon: "list-checks", title: "Compatibility", body: "The languages and frameworks Acropolis detects today." },
        { href: "/production", icon: "server", title: "Production", body: "Privileges, caches and the builder image for running it in CI." },
      ],
      cta: { href: "/introduction", label: "Read the introduction" },
    },
    art: {
      mobile: "/arte/banner.webp",
      
      agents: {
        src: "/arte/home.webp",
        alt: "Illustration: an acropolis built up in layers facing the bay of Buenos Aires",
        quote: "Every layer, verified.",
        caption: "Single binary · No daemon",
      },
    },
    features: {
      eyebrow: "The builder",
      heading: "What Acropolis does",
      more: { href: "/introduction", label: "See everything included" },
      items: [
        { icon: "file-x", title: "No Dockerfile", body: "Detects the language and framework and writes the build plan for you.", href: "/how-it-works" },
        { icon: "cpu", title: "No daemon", body: "A single Rust binary that builds in its own namespaces, without Docker or BuildKit.", href: "/how-it-works" },
        { icon: "shield-check", title: "Verified downloads", body: "Toolchains and dependencies are checked against their checksums before they are used.", href: "/agents/security" },
        { icon: "box", title: "OCI out", body: "Push to a registry, or write a tar you can load with docker load.", href: "/usage" },
        { icon: "braces", title: "Built for machines", body: "JSON plans and JSON build events, so an agent or CI can follow every step.", href: "/agents/tools" },
        { icon: "flask-conical", title: "Experimental", body: "Interfaces, output and defaults can still change between releases.", href: "/introduction#status" },
      ],
    },
  },
  es: {
    title: "Acropolis by Olimpia: código fuente adentro, imagen OCI afuera",
    description:
      "Acropolis by Olimpia: un builder experimental y sin daemon que convierte el código de una app en una imagen OCI, sin Dockerfile. Un solo binario en Rust.",
    og: { image: "/arte/og.png", alt: "Acropolis by Olimpia: una acrópolis construida en capas frente a la bahía de Buenos Aires" },
    eyebrow: "Documentación · Experimental",
    heading: "Entra el código. Sale una imagen chica.",
    lede: "Acropolis detecta el lenguaje y el framework de tu app, baja toolchains verificadas y arma una imagen OCI sin Dockerfile y sin daemon. Esta documentación tiene dos puertas: una para tu agente y otra para vos.",
    doors: { agents: "Que lo haga tu agente", humans: "Hacelo vos" },
    agents: {
      intro:
        "Acropolis no tiene un servidor MCP propio: tu agente corre la CLI y lee su JSON. Para construir y desplegar en Olimpia, conectá el servidor MCP de Olimpia y elegí Acropolis como builder.",
      clients: clients("Plan", "Build"),
      copy: "Copiar el comando de",
      machineIntro: "Esta documentación también se lee sin HTML: ",
      machine: machine("Markdown de cada página", "/es/introduction.md"),
      tasks: [
        { href: "/es/agents/connect", icon: "plug", title: "Conectar", body: "Dale a tu agente la documentación, el binario y la salida para máquinas." },
        { href: "/es/agents/tools", icon: "wrench", title: "Herramientas", body: "plan, build, inspect y cache como herramientas del agente, con su JSON y sus códigos de salida." },
        { href: "/es/agents/workflows", icon: "route", title: "Flujos", body: "Ver si una app se puede construir, reaccionar a un build fallido, corregir la detección y construir en Olimpia." },
      ],
      cta: { href: "/es/agents", label: "Guía para agentes" },
    },
    humans: {
      guides: [
        { href: "/es/install", icon: "download", title: "Instalar", body: "Binarios para Linux x86_64 y arm64, con checksums y procedencia verificable." },
        { href: "/es/how-it-works", icon: "layers", title: "Cómo funciona", body: "Detección, planes, toolchains verificadas y cómo se arma la imagen." },
        { href: "/es/usage", icon: "terminal", title: "Uso", body: "Construir a un registry o a un tar, ver el plan y pasar variables de build." },
        { href: "/es/compatibility", icon: "list-checks", title: "Compatibilidad", body: "Los lenguajes y frameworks que Acropolis detecta hoy." },
        { href: "/es/production", icon: "server", title: "Producción", body: "Privilegios, caches y la imagen del builder para correrlo en CI." },
      ],
      cta: { href: "/es/introduction", label: "Leer la introducción" },
    },
    art: {
      mobile: "/arte/banner.webp",
      
      agents: {
        src: "/arte/home.webp",
        alt: "Ilustración: una acrópolis construida en capas frente a la bahía de Buenos Aires",
        quote: "Cada capa, verificada.",
        caption: "Un binario · Sin daemon",
      },
    },
    features: {
      eyebrow: "El builder",
      heading: "Qué hace Acropolis",
      more: { href: "/es/introduction", label: "Ver todo lo que incluye" },
      items: [
        { icon: "file-x", title: "Sin Dockerfile", body: "Detecta el lenguaje y el framework y escribe el plan de build por vos.", href: "/es/how-it-works" },
        { icon: "cpu", title: "Sin daemon", body: "Un solo binario en Rust que construye en sus propios namespaces, sin Docker ni BuildKit.", href: "/es/how-it-works" },
        { icon: "shield-check", title: "Descargas verificadas", body: "Toolchains y dependencias se comparan con sus checksums antes de usarse.", href: "/es/agents/security" },
        { icon: "box", title: "Sale OCI", body: "Subí la imagen a un registry o escribí un tar para cargar con docker load.", href: "/es/usage" },
        { icon: "braces", title: "Hecho para máquinas", body: "Planes y eventos de build en JSON, para que un agente o tu CI sigan cada paso.", href: "/es/agents/tools" },
        { icon: "flask-conical", title: "Experimental", body: "La interfaz, la salida y los valores por defecto todavía pueden cambiar entre versiones.", href: "/es/introduction#status" },
      ],
    },
  },
};
