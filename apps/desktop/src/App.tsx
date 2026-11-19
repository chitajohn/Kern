import { useState } from "react";
import { NavLink, Route, Routes } from "react-router-dom";
import { SPEEDS, type Speed } from "./catalog";
import Home from "./pages/Home";
import Models from "./pages/Models";
import Project from "./pages/Project";
import Settings from "./pages/Settings";

const agents = [
  { id: "a1", name: "researcher", state: "running", branch: "kern/researcher-8f2" },
  { id: "a2", name: "builder", state: "waiting", branch: "kern/builder-1c9" },
  { id: "a3", name: "reviewer", state: "idle", branch: "kern/reviewer-4d1" },
];

export default function App() {
  return (
    <div className="app">
      <Sidebar />
      <main className="main">
        <TopBar />
        <section className="content">
          <Routes>
            <Route path="/" element={<Home />} />
            <Route path="/project/:id" element={<Project />} />
            <Route path="/models" element={<Models />} />
            <Route path="/settings" element={<Settings />} />
          </Routes>
        </section>
      </main>
      <RightRail />
    </div>
  );
}

function Sidebar() {
  return (
    <aside className="rail rail-left">
      <div className="brand">
        <span className="mark" />
        KERN
      </div>
      <nav className="nav">
        <NavLink to="/" end>
          Home
        </NavLink>
        <NavLink to="/project/kern">Project</NavLink>
        <NavLink to="/models">Models &amp; Hours</NavLink>
        <NavLink to="/settings">Settings</NavLink>
      </nav>
      <div className="section-label">Agents</div>
      <div className="agents">
        {agents.map((agent) => (
          <button className="agent" key={agent.id}>
            <span className={`dot ${agent.state}`} />
            <span className="agent-meta">
              <strong>{agent.name}</strong>
              <small>{agent.branch}</small>
            </span>
          </button>
        ))}
      </div>
      <div className="rail-foot">
        <span className="pill">local</span>
        <span className="muted">127.0.0.1:8787</span>
      </div>
    </aside>
  );
}

function TopBar() {
  const [speed, setSpeed] = useState<Speed>("slow");
  return (
    <header className="topbar">
      <div className="crumbs">
        <strong>kern</strong>
        <span className="muted">/</span>
        <span>main</span>
      </div>
      <div className="topbar-right">
        <label className="field">
          <span>Model</span>
          <select defaultValue="deepseek-v4.1-flash">
            <option value="deepseek-v4.1-flash">DeepSeek V4.1 Flash</option>
            <option value="glm-5.3-flash">GLM 5.3 Flash</option>
            <option value="solar-mini-4">Solar Mini 4</option>
            <option value="claude-haiku-5.5">Claude Haiku 5.5 · at cost</option>
          </select>
        </label>
        <div className="segmented" role="group" aria-label="Speed">
          {SPEEDS.map((item) => (
            <button
              key={item.id}
              className={speed === item.id ? "on" : ""}
              onClick={() => setSpeed(item.id)}
            >
              {item.label}
              <small>{item.minutes}m</small>
            </button>
          ))}
        </div>
        <div className="hours" title="Build hours remaining">
          <span>4h 12m</span>
          <small>left</small>
        </div>
      </div>
    </header>
  );
}

function RightRail() {
  return (
    <aside className="rail rail-right">
      <div className="section-label">Approvals</div>
      <div className="card">
        <strong>Run shell command</strong>
        <code>cargo test --workspace</code>
        <div className="row">
          <button className="btn">Allow</button>
          <button className="btn ghost">Deny</button>
        </div>
      </div>
      <div className="section-label">Checkpoints</div>
      <ol className="timeline">
        <li>
          <span>11:42</span> after tool call
        </li>
        <li>
          <span>11:38</span> step 12
        </li>
        <li>
          <span>11:31</span> start
        </li>
      </ol>
      <div className="section-label">Usage</div>
      <div className="usage">
        <div>
          <span>Input</span>
          <strong>128k</strong>
        </div>
        <div>
          <span>Output</span>
          <strong>34k</strong>
        </div>
        <div>
          <span>Cost</span>
          <strong>$0.00</strong>
        </div>
      </div>
    </aside>
  );
}
