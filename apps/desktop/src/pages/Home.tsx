import { Link } from "react-router-dom";

const projects = [
  { id: "kern", name: "Kern", path: "~/AppuBot/shi/Kern", agents: 3 },
  { id: "spear", name: "Spear", path: "~/code/spear", agents: 0 },
];

export default function Home() {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>What are we building?</h1>
          <p className="muted">Parallel agents, isolated worktrees, one local runtime.</p>
        </div>
        <button className="btn primary">New agent</button>
      </header>

      <div className="grid two">
        <section className="panel">
          <div className="panel-head">
            <h2>Recent projects</h2>
            <button className="btn ghost">Open folder</button>
          </div>
          {projects.map((project) => (
            <Link className="project-row" key={project.id} to={`/project/${project.id}`}>
              <div>
                <strong>{project.name}</strong>
                <small>{project.path}</small>
              </div>
              <span className="pill">{project.agents} agents</span>
            </Link>
          ))}
        </section>

        <section className="panel">
          <div className="panel-head">
            <h2>Live</h2>
          </div>
          <div className="live-row">
            <span className="dot running" /> researcher · editing <code>engine.rs</code>
          </div>
          <div className="live-row">
            <span className="dot waiting" /> builder · waiting for approval
          </div>
          <div className="live-row">
            <span className="dot idle" /> reviewer · idle
          </div>
        </section>
      </div>
    </div>
  );
}
