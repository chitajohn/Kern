import { MODELS, SPEEDS } from "../catalog";
import { HoursMeter } from "../components/HoursMeter";
import { HOURS_LEFT } from "../demo";

export default function Models() {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>Models &amp; hours</h1>
          <p className="muted">No daily cap. Free models by default, frontier at cost.</p>
        </div>
        <HoursMeter minutes={HOURS_LEFT} speed="slow" />
      </header>

      <section className="panel">
        <div className="panel-head">
          <h2>Speed</h2>
          <span className="muted">How long one build hour lasts</span>
        </div>
        <div className="grid three">
          {SPEEDS.map((speed) => (
            <div className="speed-card" key={speed.id}>
              <div className="row between">
                <strong>{speed.label}</strong>
                <span className="pill">{speed.burn}×</span>
              </div>
              <span className="big-num">{speed.minutes}m</span>
              <small className="muted">per build hour</small>
            </div>
          ))}
        </div>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Catalog</h2>
          <button className="btn ghost" type="button">
            Bring your own key
          </button>
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
