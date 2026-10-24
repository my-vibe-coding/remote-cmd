# remote-cmd

English | [中文](README.md)

A single-binary remote command execution tool: the server provides an HTTPS admin console and command proxy execution, while the client agent runs on the target machine — after registration approval it executes whitelisted commands remotely with live output streaming.

## Features

- **Single all-in-one binary**: the same binary acts as both server and client; web UI is embedded at compile time, zero external dependencies
- **HTTPS everywhere**: a self-signed certificate is generated automatically on first start (rustls), no extra configuration
- **Approval-based registration**: clients can only execute commands after being approved in the admin console
- **Command whitelist**: the server only runs programs on the whitelist, arbitrary commands are rejected
- **Live output**: SSE-based streaming, output is rendered line by line in real time with exit codes preserved
- **Aliases with templates**: an alias maps to a command template (e.g. `git add {args}`) with transparent argument passing
- **Executable script shims**: generate a script per alias into the bin directory so it can be invoked from any terminal; Ctrl+C terminates directly with no confirmation prompt
- **Audit log**: every execution is recorded and traceable
- **Public address display**: the server fetches its public IPv4 via ipv4.ip.sb on startup for convenient external access

## Quick Start

### Download prebuilt binaries

Grab the package for your platform from [GitHub Releases](https://github.com/my-vibe-coding/remote-cmd/releases):

- `remote-cmd-<version>-x86_64-pc-windows-msvc.zip` (Windows)
- `remote-cmd-<version>-x86_64-unknown-linux-musl.tar.gz` (statically linked, runs on Alpine and other distros out of the box)

### Start the server

```bash
remote-cmd serve              # port 3233 by default, prints an auto-generated admin password
remote-cmd serve --port 8443 --password mypass
```

Then open the admin console at `https://<host>:3233/admin`.

### Start the client

```bash
remote-cmd                    # opens the local page at http://127.0.0.1:3234
```

Register to the server in the page → approve it in the admin console → run whitelisted commands remotely from the "Run" page.

## Using Aliases

Create an alias in the client page, e.g. `gadd` → template `git add {args}`:

```bash
# Generate executable scripts (all aliases by default)
remote-cmd gen-script

# Then invoke from any terminal (add the bin directory to PATH)
gadd -A
```

Scripts live in `%USERPROFILE%\.remote-cmd\bin` as copies of the main binary named after each alias; on startup the copy detects its own identity and runs the remote command for that alias. Unlike a traditional `.bat` wrapper, Ctrl+C terminates immediately and the exit code is preserved.

Alternatively, skip the script entirely:

```bash
remote-cmd run gadd -A
```

## CLI Reference

| Command | Description |
|---|---|
| `remote-cmd` | Run in client mode, opens the local page |
| `remote-cmd serve` | Run in server mode (`--port`, `--password`, `--data-dir`) |
| `remote-cmd run <alias> [args...]` | Run the remote command bound to an alias |
| `remote-cmd gen-script [alias]` | Generate alias scripts (all aliases if omitted) |

Data directories:

- Server: `%USERPROFILE%\.remote-cmd\server` (override with `--data-dir`)
- Client: `%USERPROFILE%\.remote-cmd\client`

## Building from Source

```bash
# Frontend (embedded at compile time, must be built first)
cd web
npm ci
npm run build

# Rust
cargo build --release
```

Pushing a `v*` tag triggers GitHub Actions to build Windows and Alpine (musl) artifacts and upload them to the Release.

## Security Model

- Admin password is stored as an Argon2 hash; reset it with `--password` if forgotten
- Client registration requires admin approval; credentials are bound to a fingerprint
- Only whitelisted programs are executed
- All traffic goes over HTTPS (self-signed certificate); executions are written to the audit log

## Tech Stack

- Backend: Rust · axum · rustls(ring) · SSE · Vue 3 + Vite (embedded)
- Windows-native capabilities: `SHOpenFolderAndSelectItems` (reveal in Explorer), OEM code page decoding (no mojibake for Chinese output)

## License

MIT
