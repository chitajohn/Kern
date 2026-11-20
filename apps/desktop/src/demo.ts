import type { AgentState } from "./components/StateChip";

/**
 * Placeholder data until M1 wires the local API. Every screen reads from
 * here, so replacing these exports with TanStack Query hooks is the whole
 * integration.
 */

export type Project = { id: string; name: string; path: string; agents: number };

export const PROJECTS: Project[] = [
  { id: "kern", name: "Kern", path: "~/AppuBot/shi/Kern", agents: 3 },
  { id: "spear", name: "Spear", path: "~/code/spear", agents: 0 },
];

export type Agent = {
  id: string;
  name: string;
  state: AgentState;
  task: string;
  branch: string;
  progress: number;
};

export const AGENTS: Agent[] = [
  {
    id: "a1",
    name: "researcher",
    state: "running",
    task: "Map the engine loop",
    branch: "kern/researcher-8f2",
    progress: 62,
  },
  {
    id: "a2",
    name: "builder",
    state: "waiting",
    task: "Add the project model",
    branch: "kern/builder-1c9",
    progress: 28,
  },
  {
    id: "a3",
    name: "reviewer",
    state: "idle",
    task: "Review PR #2",
    branch: "kern/reviewer-4d1",
    progress: 0,
  },
];

export const THREAD = [
  { role: "user", text: "Find where checkpoints are written." },
  {
    role: "agent",
    text: "Reading the engine loop and the checkpoint store.",
    tool: { name: "filesystem.read", result: "ok" },
  },
  {
    role: "agent",
    text: "Checkpoints are committed in CheckpointStore::save after every safe point.",
  },
];

export const CHECKPOINTS = [
  { time: "11:42", label: "after tool call" },
  { time: "11:38", label: "step 12" },
  { time: "11:31", label: "start" },
];

export const USAGE = { input: "128k", output: "34k", cost: "$0.00" };

export const HOURS_LEFT = 252;

export const TODAY = [
  { label: "Hours used", value: "48m" },
  { label: "Tool calls", value: "132" },
  { label: "Tokens", value: "162k" },
  { label: "Spent", value: "$0.00" },
];

export type Run = {
  id: string;
  agent: string;
  project: string;
  model: string;
  duration: string;
  state: AgentState;
};

export const RUNS: Run[] = [
  {
    id: "r1",
    agent: "researcher",
    project: "Kern",
    model: "DeepSeek V4.1 Flash",
    duration: "6m",
    state: "running",
  },
  {
    id: "r2",
    agent: "builder",
    project: "Kern",
    model: "GLM 5.3 Flash",
    duration: "11m",
    state: "waiting",
  },
  {
    id: "r3",
    agent: "reviewer",
    project: "Kern",
    model: "Solar Mini 4",
    duration: "3m",
    state: "idle",
  },
];
