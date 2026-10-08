import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  /* config options here */
  transpilePackages: ["@repo/ui"],
  cacheComponents: true,
  partialPrefetching: true,
};

export default nextConfig;
