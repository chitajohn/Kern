export default function Settings() {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>Settings</h1>
          <p className="muted">Local runtime, providers, updates.</p>
        </div>
      </header>

      <div className="grid two">
        <section className="panel">
          <div className="panel-head">
            <h2>Providers</h2>
          </div>
          <div className="setting">
            <span>Runtime</span>
            <code>127.0.0.1:8787</code>
          </div>
          <div className="setting">
            <span>Ollama</span>
            <code>http://127.0.0.1:11434</code>
          </div>
          <div className="setting">
            <span>Frontier keys</span>
            <button className="btn ghost" type="button">
              Add key
            </button>
          </div>
        </section>

        <section className="panel">
          <div className="panel-head">
            <h2>App</h2>
          </div>
          <div className="setting">
            <span>Theme</span>
            <span>Ink</span>
          </div>
          <div className="setting">
            <span>Updates</span>
            <span className="pill good">automatic</span>
          </div>
          <div className="setting">
            <span>Version</span>
            <code>0.1.0</code>
          </div>
        </section>
      </div>
    </div>
  );
}
