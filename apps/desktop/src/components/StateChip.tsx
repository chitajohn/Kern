export type AgentState = "running" | "waiting" | "idle" | "failed";

const LABELS: Record<AgentState, string> = {
  running: "running",
  waiting: "needs you",
  idle: "idle",
  failed: "failed",
};

/** State is never colour alone: the chip always carries the word. */
export function StateChip({ state }: { state: AgentState }) {
  return (
    <span className={`chip ${state}`}>
      <i />
      {LABELS[state]}
    </span>
  );
}
