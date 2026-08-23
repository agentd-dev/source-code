// The editor page is a client component; its metadata lives here.
export const metadata = {
  title: "Config editor — agentd",
  description:
    "A visual editor for agentd configuration: compose workflows, MCP servers and lifecycle settings, and export validated YAML.",
  alternates: { canonical: "/editor/" },
};

export default function EditorLayout({ children }) {
  return children;
}
