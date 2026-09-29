// https://agentd.dev/a2a/binding/<name>/ — a protocol binding's
// specification, at its URI. The names come from web/lib/extensions.json
// (see ../../spec.jsx).
import SpecPage, { specEntry, specMetadata, specParams } from "../../spec";

export const dynamicParams = false;

export function generateStaticParams() {
  return specParams("binding");
}

export async function generateMetadata({ params }) {
  const { name } = await params;
  return specMetadata(specEntry("binding", name));
}

export default async function BindingSpec({ params }) {
  const { name } = await params;
  return <SpecPage entry={specEntry("binding", name)} />;
}
