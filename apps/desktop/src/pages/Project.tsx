import { useParams } from "react-router-dom";

const board = [
  {
    name: "researcher",
    task: "Map the engine loop",
    state: "running",
    branch: "kern/researcher-8f2",
    progress: 62,
  },
  {
    name: "builder",
    task: "Add project model",
    state: "waiting",
    branch: "kern/builder-1c9",
    progress: 28,
  },
  {
    name: "reviewer",
    task: "Review PR #2",
    state: "idle",
    branch: "kern/reviewer-4d1",
    progress: 0,
  },
];

export default function Project() {
  const { id } = useParams();
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>{id}</h1>
          <p className="muted">3 worktrees · main · local</p>
        </div>
        <button className="btn primary">New agent</button>
      </header>

      <div className="board">
        {board.map((agent) => (
          <article className="panel agent-card" key={agent.name}>
            <div className="panel-head">
              <h2>{agent.name}</h2>
              <span className={`dot ${agent.state}`} />
            </div>
            <p>{agent.task}</p>
            <code>{agent.branch}</code>
            <div className="bar">
              <span style={{ width: `${agent.progress}%` }} />
            </div>
            <div className="row between">
              <small className="muted">{agent.state}</small>
              <button className="btn ghost">Open</button>
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
          <div className="msg user">Find where checkpoints are written.</div>
          <div className="msg agent">
            Reading <code>crates/kern-core/src/checkpoint.rs</code> and the engine loop.
            <div className="tool">
              tool · filesystem.read <span className="ok">ok</span>
            </div>
          </div>
          <div className="msg agent">
            Checkpoints are committed in <code>CheckpointStore::save</code> after every safe point.
          </div>
        </div>
      </section>
    </div>
  );
}
