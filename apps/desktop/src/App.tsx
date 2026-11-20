import { useState } from "react";
import { NavLink, Route, Routes } from "react-router-dom";
import { SPEEDS, type Speed } from "./catalog";
import { HoursMeter } from "./components/HoursMeter";
import { Mark } from "./components/Mark";
import { StateChip } from "./components/StateChip";
import { AGENTS, CHECKPOINTS, HOURS_LEFT, PROJECTS, USAGE } from "./demo";
import Home from "./pages/Home";
import Models from "./pages/Models";
import Project from "./pages/Project";
import Settings from "./pages/Settings";

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
        <Mark size={20} />
        <span className="wordmark">KERN</span>
      </div>

      <nav className="nav">
        <NavLink to="/" end>
          Home
        </NavLink>
        <NavLink to={`/project/${PROJECTS[0].id}`}>Projects</NavLink>
        <NavLink to="/models">Models &amp; hours</NavLink>
        <NavLink to="/settings">Settings</NavLink>
      </nav>

      <div className="section-label">Agents</div>
      <div className="agents">
        {AGENTS.map((agent) => (
          <button className="agent" key={agent.id} type="button">
            <span className="agent-meta">
              <strong>{agent.name}</strong>
              <small>{agent.branch}</small>
            </span>
            <StateChip state={agent.state} />
          </button>
        ))}
      </div>

      <div className="rail-foot">
        <span className="pill live">local</span>
        <code>127.0.0.1:8787</code>
      </div>
    </aside>
  );
}

function TopBar() {
  const [speed, setSpeed] = useState<Speed>("slow");
  return (
    <header className="topbar">
      <div className="crumbs">
        <strong>{PROJECTS[0].name.toLowerCase()}</strong>
        <span className="slash">/</span>
        <span className="muted">main</span>
      </div>

      <div className="topbar-right">
        <label className="field">
          <span>Model</span>
          <select defaultValue="deepseek-v4.1-flash">
            <option value="deepseek-v4.1-flash">DeepSeek V4.1 Flash</option>
            <option value="glm-5.3-flash">GLM 5.3 Flash</option>
            <option value="solar-mini-4">Solar Mini 4</option>
            <option value="nemotron-3.5-lightning">Nemotron 3.5 Lightning</option>
            <option value="claude-haiku-5.5">Claude Haiku 5.5 · at cost</option>
          </select>
        </label>

        <div className="segmented" role="group" aria-label="Speed">
          {SPEEDS.map((item) => (
            <button
              key={item.id}
              type="button"
              className={speed === item.id ? "on" : ""}
              onClick={() => setSpeed(item.id)}
              title={`${item.label}: one build hour lasts ${item.minutes} minutes`}
            >
              {item.label}
              <small>{item.minutes}m</small>
            </button>
          ))}
        </div>

        <HoursMeter minutes={HOURS_LEFT} speed={speed} compact />
      </div>
    </header>
  );
}

function RightRail() {
  return (
    <aside className="rail rail-right">
      <div className="section-label">Approvals</div>
      <div className="card">
        <div className="card-head">
          <strong>Run shell command</strong>
          <span className="pill">builder</span>
        </div>
        <code className="cmd">cargo test --workspace</code>
        <div className="row">
          <button className="btn primary" type="button">
            Allow
          </button>
          <button className="btn ghost" type="button">
            Deny
          </button>
        </div>
      </div>

      <div className="section-label">Checkpoints</div>
      <ol className="timeline">
        {CHECKPOINTS.map((item) => (
          <li key={item.time}>
            <span>{item.time}</span>
            {item.label}
          </li>
        ))}
      </ol>

      <div className="section-label">Usage</div>
      <div className="usage">
        <div>
          <span>Input</span>
          <strong>{USAGE.input}</strong>
        </div>
        <div>
          <span>Output</span>
          <strong>{USAGE.output}</strong>
        </div>
        <div>
          <span>Cost</span>
          <strong>{USAGE.cost}</strong>
        </div>
      </div>
    </aside>
  );
}
