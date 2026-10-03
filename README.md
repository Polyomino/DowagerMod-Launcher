# DowagerMod Launcher

Fast-starting desktop app (Rust + egui) that manages which version of
[DowagerMod](https://github.com/siegelh/DowagerMod) — a
Sid Meier's Civilization IV: Beyond the Sword mod — is installed on the system.

## What it does

- **Installed version card** — on launch it shows the checked-out branch,
  the installed commit (hash, subject, date), and how far that commit is
  behind/ahead of the branch tip (`Update to latest` fast-forwards it to
  the remote tip, no upstream config required).
- **Branch list** — all local and remote branches ordered by latest commit
  time, with a keyboard-navigable filter (type to filter, `Up`/`Down` to
  move, `Enter` to switch, double-click also switches).
- **Session log** — every status line plus every git invocation (with
  timing and failure output) is appended to
  `%LOCALAPPDATA%\DowagerMod-Launcher\launcher.log` (rotates past 1 MB);
  open it from the Settings panel.
- **Deploy to Civ4** — runs `Install DowagerMod.bat` from the checkout
  (UAC prompt appears; required after pulling, since `git pull` alone does
  not touch the live game install). Refuses while Civ4 is running, and
  blocks second deploys (even from another launcher window), launches,
  and git jobs until the installer ends.
- **Launch Civ4 with DowagerMod** — starts the live
  `Beyond the Sword\Civ4BeyondSword.exe` detected via the installer's
  `%LOCALAPPDATA%\DowagerMod\config.json`, a Steam library scan, or the
  `steam://run/8800` fallback. The panel shows whether the live install
  matches the current checkout (compares `last_mod_version` with the
  checkout's `git describe`).
- **Settings** — the mod repo path is configurable and persisted to
  `%APPDATA%\DowagerMod-Launcher\config.json` (auto-detects a `DowagerMod`
  folder next to the launcher otherwise).

## Prerequisites

- Windows 10/11, 64-bit
- [Rust stable](https://rustup.rs/) (1.80+) with a linker:
  `winget install Rustlang.Rustup`, then install
  [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)
  (Desktop development with C++), or use the `x86_64-pc-windows-gnu` toolchain
- `git` on `PATH`
- A DowagerMod checkout (sibling `DowagerMod` folder by default)

## Run / build

```powershell
cargo run --release
cargo test
```

## Publish to GitHub

```powershell
cd DowagerMod-Launcher
git init
git add .
git commit -m "Initial DowagerMod launcher"
gh repo create DowagerMod-Launcher --public --source=. --push
```

## License

MIT — see `LICENSE-MIT`.
