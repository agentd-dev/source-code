export const dynamic = "force-static";

// /robots.txt — everything is public; point crawlers at the sitemap.
export default function robots() {
  return {
    rules: [{ userAgent: "*", allow: "/" }],
    sitemap: "https://agentd.dev/sitemap.xml",
  };
}
