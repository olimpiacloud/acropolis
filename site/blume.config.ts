import { defineConfig } from "blume";
import { family } from "./family";

export default defineConfig({
  ...family("acropolis"),
  title: "Acropolis by Olimpia",
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
  agents: {
    llmsTxt: {
      details:
        "Acropolis is an experimental app-to-OCI-image builder by Olimpia (https://olimpia.dev): a single Rust binary (`acropolis`) for Linux x86_64 and arm64 that detects the language and framework, downloads verified toolchains and dependencies, compiles, and assembles the image with no Dockerfile and no daemon. `acropolis build ./app -t registry/app:tag` pushes to a registry, `--oci image.tar` writes a tar for `docker load`, `acropolis plan ./app` prints the build plan (`--json` for machines, `--events json` for machine-readable progress events). Source: https://github.com/olimpiacloud/acropolis (Apache-2.0 or MIT).",
    },
  },
});
