# DeepSeek Harness for Linux (Rust/Tauri)

A native Linux desktop shell for DeepSeek Harness, written in Rust with Tauri 2 and WebKitGTK. Upstream only ships desktop builds for macOS and Windows (Electron, `apps/desktop`). This app lives in the `ndecool/deepseek-harness` fork and does not touch any upstream file, so upstream merges stay free of conflicts.

## What it does

- Starts `dsh web --no-open --host 127.0.0.1 --port 0` in its own process group and reads the tokenized URL from the `dsh web:` startup line. The window then loads that URL; the browser cookie authenticates the session and the token is dropped from the address.
- Shows a local status page while the runtime starts. If the runtime crashes, fails to start, or prints no URL within 180 s, the page shows the error, the last 200 log lines, **Restart runtime** and **Open log**.
- Reads the environment from your login shell, the same way the Electron app does (`apps/desktop/src/login-shell-environment.ts`). A launch from the application menu therefore sees your `PATH`, `DEEPSEEK_API_KEY` and the rest. `DSH_*` variables are not taken from the shell.
- Runs as a single instance. A second launch, or a `dsh://open` link, brings the existing window to the front.
- Opens external links (`target=_blank` or another origin) in the system browser. Only the status page and the running dsh origin load inside the app.
- Adds a tray icon (Show, Restart runtime, Open runtime log, Quit) when libayatana-appindicator is installed. Closing the window quits the app and shuts dsh down: `SIGTERM` to the process group, then `SIGKILL` after 10 s.
- Writes the runtime log to `$XDG_STATE_HOME/deepseek-harness/dsh.log` (default `~/.local/state/…`), mode 0600, with tokens redacted. The previous run's log is kept as `dsh.log.1`.
- Remembers window size and position.
- With the NVIDIA proprietary driver, sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` to avoid WebKitGTK's blank-window bug.

The remote dsh page gets **no** Tauri IPC access. The capability in `capabilities/main.json` only applies to the local status page.

## Runtime resolution

The first match wins:

1. `DSH_LINUX_COMMAND`: path to a `dsh` executable.
2. The bundled runtime: `resources/runtime/node/bin/node` plus `resources/runtime/dsh/node_modules/@deepseek-ai/dsh/lib/bin.js`.
3. Debug builds only: this checkout's `apps/cli/lib/bin.js` (after `pnpm run build`), run with `node` from `PATH`.
4. `dsh` on `PATH` (`npm install -g @deepseek-ai/dsh`).

## Build

Prerequisites (Ubuntu/Debian):

```sh
sudo apt install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev build-essential curl
cargo install tauri-cli --version '^2' --locked
```

Node ≥ 22.19 (root `package.json` `engines`) must be on `PATH`; `bundle-runtime.sh` uses it to read lock files.

```sh
cd apps/desktop-linux
cargo tauri dev                       # development; uses the workspace checkout or dsh on PATH
./scripts/bundle-runtime.sh           # stage pinned Node + @deepseek-ai/dsh into resources/runtime
cargo tauri build --config tauri.bundle.json   # AppImage + .deb in target/release/bundle/
cargo test                            # parser / redaction / shell-env unit tests
```

`bundle-runtime.sh` downloads the Node version pinned in `scripts/primary-runtime/lock.json` and checks its SHA-256. It then installs `@deepseek-ai/dsh@<version of apps/cli/package.json>` from npm. To ship changes made in this fork, set `DSH_LINUX_DSH_SPEC` to a tarball built with `pnpm run release:pack`.

## Keeping up with upstream

```sh
git fetch upstream
git merge upstream/master        # conflict-free: this app only adds files
./apps/desktop-linux/scripts/bundle-runtime.sh   # picks up the new pinned Node and dsh version
```

The `desktop-linux` workflow (`.github/workflows/desktop-linux.yml`) runs this merge on `master` once a day (or on demand via *Run workflow*), then builds the AppImage and .deb as artifacts; pushing a `desktop-linux-v*` tag also publishes a GitHub release. Upstream often edits its own workflow files, which `GITHUB_TOKEN` may not push: add a repository secret `SYNC_TOKEN` (fine-grained PAT with *Contents* and *Workflows* write on the fork). Actions must be enabled on the fork once (Actions tab).

## Known limits

- Voice input: WebKitGTK media permission prompts are not wired up yet, so the microphone plugin cannot record.
- No auto-update yet. Rebuild, or install the new .deb/AppImage from the workflow artifacts.
- DeepSeek account sign-in uses the Web UI flow (`web` profile), not the Electron app's embedded Platform view.
