// https://agentd.dev/a2a/ext/<name>/ — an extension's specification, at its
// URI. The names come from web/lib/extensions.json (see ../../spec.jsx).
import SpecPage, { specEntry, specMetadata, specParams } from "../../spec";

export const dynamicParams = false;

export function generateStaticParams() {
  return specParams("extension");
}

export async function generateMetadata({ params }) {
  const { name } = await params;
  return specMetadata(specEntry("extension", name));
}

export default async function ExtensionSpec({ params }) {
  const { name } = await params;
  return <SpecPage entry={specEntry("extension", name)} />;
}
