# Kern Desktop

The Kern desktop app: Tauri 2 + React/TypeScript. The Rust daemon stays the engine;
this app is the product surface.

## Develop

```bash
npm install
npm run dev        # browser at http://localhost:1420
npm run tauri dev  # native window (needs the platform webview deps)
```

## Build

```bash
npm run build
npm run tauri build
```

The Windows `.exe` is built by `.github/workflows/desktop.yml` on `windows-latest`
(NSIS installer, uploaded as the `kern-windows-x64` artifact).

## Layout

- `src/` — React shell: home, project board, models and hours, settings
- `src/catalog.ts` — free model catalog and slow/medium/max burn rates
- `src-tauri/` — Tauri shell around the web UI
