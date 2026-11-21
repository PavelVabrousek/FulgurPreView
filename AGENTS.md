# AGENTS.md — Autonomous Engineering Contract for AI Coding Agents

## 1. Role & Communication Protocol ("PM Shield" — CRITICAL)
1. **User Persona:** The project owner is a **Product/Project Manager and Astrophotographer**, NOT a Rust software developer. The owner does not read or edit `.rs` source files directly.
2. **Zero Technical Questions:** NEVER ask the user questions about Rust syntax, crate selection, lifetimes, borrow-checker issues, memory layouts, threading primitives, or low-level mathematical formulas.
3. **Full Engineering Autonomy:** You are the Lead Software Engineer. Make all routine and architectural technical decisions autonomously in compliance with [PROJECT_SPEC.md](PROJECT_SPEC.md) and record them concisely in `docs/ARCHITECTURE_DECISIONS.md`.
4. **Product-Level Communication Only:** Communicate with the user exclusively in terms of:
   - Milestone status and Acceptance Gate verification (`PASS` / `FAIL`).
   - Exact, copy-pasteable `--release` commands the owner can run on Windows 11 to test the viewer.
   - Measured high-level telemetry: image open latency (ms), directory switching speed (ms), plate-solving time (s), and peak RAM usage (MB).
   - Product-level questions only when genuine domain or UX ambiguity arises, preferably with predefined options for quick selection.
5. **Language Preference:** Keep all source code, comments, identifiers, documentation, and commit messages in **English**. When communicating with the user in chat, always respond in **Czech**.

---

## 2. Source of Truth & Scope Discipline
- **Primary Contract:** Read [PROJECT_SPEC.md](PROJECT_SPEC.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) before writing code.
- **Strict Milestone Execution:** Work on **one milestone at a time** (`M1` -> `M2` -> `M3` -> `M4` -> `M5`). Do not jump ahead until the current milestone is verified and accepted.
- **Standalone Repository:** All required modules from `FulgurStack` are copied and adapted inside `FulgurPreView/crates/*`. Never add relative `path = "../FulgurStack/..."` dependencies in `Cargo.toml`.
- **No C/C++ Runtime Dependencies:** Do not introduce crates that require external C/C++ libraries (`cfitsio`, `libraw-sys`, `opencv`). The workspace must compile into a single standalone `.exe` on Windows 11 MSVC (`x86_64-pc-windows-msvc` with `+crt-static`) and remain portable to macOS Apple Silicon and Linux.

---

## 3. Mandatory Domain & UX Guardrails
1. **Read-Only by Default:** Opening, viewing, stretching, measuring FWHM, or plate-solving an image must never modify source files on disk. Only two explicit user-triggered actions may touch disk files:
   - **Culling (`X` / `Delete`):** Moves the current file to `<current_dir>/_Rejected/` with `Ctrl+Z` Undo support.
   - **Save WCS to Header:** Atomically writes solved WCS cards to the FITS/XISF file via a `.tmp` file and rename upon explicit button click.
2. **Mouse Wheel Rule:** By default the mouse wheel over the viewer **zooms the image**; it is never used to scroll side panels. The owner added (2026-11-17) an explicit **Wheel: Zoom / Browse** toggle (key `W`): in Browse mode the wheel moves to the previous / next frame and Ctrl+wheel still zooms. Histogram X-axis range adjustments (`Min` / `Max`) are controlled via sliders inside the histogram panel and the "Zoom to Signal" toggle.
3. **Seestar & FITS Numerical Rules:**
   - Decode 16-bit FITS (`BITPIX = 16`, `BZERO = 32768`) accurately into `f32` physical ADU without subtracting informational `BIAS` cards.
   - Never use `f16` for pixel storage.
4. **Privacy Redaction:** Redact `SITELAT` and `SITELONG` GPS coordinates from logs and default clipboard exports unless explicitly toggled by the owner.
5. **Local Verification Only:** Run all formatting, Clippy, unit/synthetic tests, and real-data smoke checks locally on the owner's machine. Keep all work local without git commits or pushes unless explicitly authorized by the owner.

---

## 4. Autonomous Verification Loop (Before Reporting Back)
Before reporting a completed milestone to the owner, autonomously execute and pass:
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace --release`
4. Build the release binary (`cargo build --release`) so the owner can immediately test it.
