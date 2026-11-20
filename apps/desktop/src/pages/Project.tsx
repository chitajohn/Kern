import { useParams } from "react-router-dom";
import { StateChip } from "../components/StateChip";
import { AGENTS, THREAD } from "../demo";

export default function Project() {
  const { id } = useParams();
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>{id}</h1>
          <p className="muted">3 worktrees · main · local</p>
        </div>
        <button className="btn primary" type="button">
          New agent
        </button>
      </header>

      <div className="board">
        {AGENTS.map((agent) => (
          <article className="panel agent-card" key={agent.id}>
            <div className="panel-head">
              <h2>{agent.name}</h2>
              <StateChip state={agent.state} />
            </div>
            <p>{agent.task}</p>
            <code className="branch">{agent.branch}</code>
            <div className="meter">
              <span style={{ width: `${agent.progress}%` }} />
            </div>
            <div className="row between">
              <small className="muted">{agent.progress}% · 12 tool calls</small>
              <button className="btn ghost" type="button">
                Open
              </button>
            </div>
          </article>
        ))}
      </div>

      <section className="panel">
        <div className="panel-head">
          <h2>Thread</h2>
          <span className="pill">researcher</span>
        </div>
        <div className="thread">
          {THREAD.map((message, index) => (
            <div className={`msg ${message.role}`} key={index}>
              {message.text}
              {message.tool && (
                <div className="tool">
                  tool · {message.tool.name} <span className="ok">{message.tool.result}</span>
                </div>
              )}
            </div>
          ))}
        </div>
      </section>
    </div>
  );
}
