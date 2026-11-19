import { MODELS, SPEEDS } from "../catalog";

export default function Models() {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>Models &amp; Hours</h1>
          <p className="muted">Free models by default. Frontier at cost. No daily cap.</p>
        </div>
        <div className="hours big">
          <span>4h 12m</span>
          <small>remaining today</small>
        </div>
      </header>

      <section className="panel">
        <div className="panel-head">
          <h2>Speed</h2>
          <span className="muted">How long an hour lasts</span>
        </div>
        <div className="grid three">
          {SPEEDS.map((speed) => (
            <div className="speed-card" key={speed.id}>
              <strong>{speed.label}</strong>
              <span className="big-num">{speed.minutes}m</span>
              <small>burn {speed.burn}×</small>
            </div>
          ))}
        </div>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Catalog</h2>
          <button className="btn ghost">Bring your own key</button>
        </div>
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              <th>Tier</th>
              <th>Notes</th>
            </tr>
          </thead>
          <tbody>
            {MODELS.map((model) => (
              <tr key={model.id}>
                <td>
                  <strong>{model.name}</strong>
                </td>
                <td>
                  <span className={model.tier === "free" ? "pill good" : "pill"}>
                    {model.tier === "free" ? "free" : "at cost"}
                  </span>
                </td>
                <td className="muted">{model.note}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  );
}
