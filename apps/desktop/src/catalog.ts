export type Speed = "slow" | "medium" | "max";

export type Model = {
  id: string;
  name: string;
  tier: "free" | "cost";
  note: string;
};

export const SPEEDS: { id: Speed; label: string; burn: number; minutes: number }[] = [
  { id: "slow", label: "Slow", burn: 1, minutes: 60 },
  { id: "medium", label: "Medium", burn: 2, minutes: 30 },
  { id: "max", label: "Max", burn: 4, minutes: 15 },
];

export const MODELS: Model[] = [
  {
    id: "deepseek-v4.1-flash",
    name: "DeepSeek V4.1 Flash",
    tier: "free",
    note: "Fast coding and tool use, 1M context",
  },
  { id: "glm-5.3-flash", name: "GLM 5.3 Flash", tier: "free", note: "Experimental, 256K context" },
  { id: "solar-mini-4", name: "Solar Mini 4", tier: "free", note: "Compact, 500K context" },
  {
    id: "nemotron-3.5-lightning",
    name: "Nemotron 3.5 Lightning",
    tier: "free",
    note: "Experimental, 256K context",
  },
  { id: "claude-haiku-5.5", name: "Claude Haiku 5.5", tier: "cost", note: "100K context, at cost" },
  { id: "gpt-6-luna", name: "GPT-6 Luna", tier: "cost", note: "1M context, at cost" },
];
