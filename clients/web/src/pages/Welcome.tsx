import { Sparkles } from "lucide-react";
import { EmptyState } from "../components/EmptyState";

export function Welcome() {
  return (
    <EmptyState icon={<Sparkles size={24} />} title="Welcome to horsie">
      Select a session from the sidebar, or create a new one to start working
      with an agent in a sandboxed runtime.
    </EmptyState>
  );
}
