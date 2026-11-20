import { Link } from "react-router-dom";
import { StateChip } from "../components/StateChip";
import { AGENTS, PROJECTS, RUNS, TODAY } from "../demo";

export default function Home() {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>What are we building?</h1>
          <p className="muted">
            Parallel agents in isolated worktrees. Free models by default, frontier at cost.
          </p>
        </div>
        <button className="btn primary" type="button">
          New agent
        </button>
      </header>

      <div className="grid two">
        <section className="panel">
          <div className="panel-head">
            <h2>Projects</h2>
            <button className="btn ghost" type="button">
              Open folder
            </button>
          </div>
          {PROJECTS.map((project) => (
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
            <span className="pill live">3 running</span>
          </div>
          {AGENTS.map((agent) => (
            <div className="live-row" key={agent.id}>
              <div>
                <strong>{agent.name}</strong>
                <small className="muted">{agent.task}</small>
              </div>
              <StateChip state={agent.state} />
            </div>
          ))}
        </section>
      </div>

      <section className="panel">
        <div className="panel-head">
          <h2>Today</h2>
          <span className="muted">local · no account</span>
        </div>
        <div className="stats">
          {TODAY.map((stat) => (
            <div key={stat.label}>
              <span>{stat.label}</span>
              <strong>{stat.value}</strong>
            </div>
          ))}
        </div>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Recent runs</h2>
        </div>
        <table className="table">
          <thead>
            <tr>
              <th>Agent</th>
              <th>Project</th>
              <th>Model</th>
              <th>Ran</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {RUNS.map((run) => (
              <tr key={run.id}>
                <td>
                  <strong>{run.agent}</strong>
                </td>
                <td className="muted">{run.project}</td>
                <td className="muted">{run.model}</td>
                <td className="muted">{run.duration}</td>
                <td className="right">
                  <StateChip state={run.state} />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>

      <section className="panel ad">
        <span className="section-label inline">Sponsored</span>
        <p>
          <strong>Neon Postgres</strong> — branches per agent, one per worktree. Spin one up in
          sixteen seconds.
        </p>
        <button className="btn ghost" type="button">
          Open
        </button>
      </section>
    </div>
  );
}
