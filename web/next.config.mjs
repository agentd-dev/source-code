/** @type {import('next').NextConfig} */
const repoBase = process.env.PAGES_BASE_PATH ?? "";

const nextConfig = {
  output: "export",
  basePath: repoBase,
  assetPrefix: repoBase,
  trailingSlash: true,
  images: { unoptimized: true },
};

export default nextConfig;
