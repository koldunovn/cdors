# Agent check (Task 14)

Status 2026-10-09: done. Ten headless Claude Code sessions, five analysis tasks (`bench/agent_tasks.md`) times two
arms: arm A with cdors, arm B with cdo 2.6.0 plus Python (xarray, zarr, numpy, healpy) and ncdump. **All ten
answers are correct.** With cdors, the agents finished the five tasks in 704 s of wall time, against 1684 s with
cdo and Python. In return they made more tool calls and used more tokens, because they had to learn cdors from
its documentation.

## Setup

- `claude -p` 2.1.295, default model (`claude-opus-5-5`) and effort, one session after another on a login node,
  each in a fresh directory, at most 16 threads, no batch jobs, 50 turns and 30 min at most. Tools: Bash, Read,
  Write, Edit, Glob, Grep; `rm`, `rmdir`, `git`, `sbatch`, `srun`, `salloc`, `scancel` denied. The full settings
  are in `bench/agent_tasks.md` ("Running the check") and `bench/agent_check.sh`.
- Arm A had the cdors binary of commit 0415117 (a frozen copy), `cdors help`, `cdors ops`, and the README and
  `docs/deviations.md` copied into the session directory. Arm B had cdo, the Python environment and ncdump on
  its PATH. The prompts are the same apart from the paragraph on tools.
- Reference answers were computed beforehand with xarray/numpy and cdo, without cdors.
- Runs: the pilot `20261009-181750` (T5-A, 18:17), then `20261009-190040-main` (T1–T4, both arms, 19:00–19:34)
  and `20261009-190040-t5b` (T5-B, until 19:38). Transcripts: `/scratch/a/a270088/cdors-agentcheck/<run>/<task>-<arm>/`.
  The login node was shared with other work, including my own builds and tests from 19:19 on, so the wall times
  are indicative.

## Results

`bench/agent/rescored/` holds the scores after two scorer fixes (see below); `bench/agent/results-*.md` are the
runner's original scores. Wall time is the whole session; USD is the API list price the CLI reports.

| Task | What | Arm A: cdors | Arm B: cdo + Python |
|---|---|---|---|
| T1 | July of the 2020–2024 climatology, global mean (HEALPix z9 daily Zarr) | correct; 12 turns, 11 tool calls, 127 s, $0.43 | correct; 8 turns, 7 tool calls, 781 s, $0.22 |
| T2 | Annual means 1950–1954, North-Atlantic box (0.25°, kerchunk Parquet / raw NetCDF) | correct; 20, 19, 245 s, $0.73 | correct; 12, 11, 373 s, $0.48 |
| T3 | Bilinear to 1°, time mean, global mean (ocean `to`, January 1950) | correct; 18, 17, 95 s, $0.61 | correct; 13, 12, 199 s, $0.49 |
| T4 | 95th percentile at Hamburg, 2020 (HEALPix z9 3-hourly Zarr) | correct; 14, 13, 96 s, $0.54 | correct; 4, 3, 65 s, $0.15 |
| T5 | January 1950 European box mean, from the EERIE cloud | correct; 10, 9, 141 s, $0.41 | correct; 8, 7, 266 s, $0.23 |
| **All** | | **5 of 5; 69 tool calls, 704 s; 236k fresh + 2.19M cache-read tokens; $2.73** | **5 of 5; 40 tool calls, 1684 s; 127k fresh + 0.78M cache-read tokens; $1.57** |

- **Reliability.** No arm-A tool call failed, and every arm-A session ran `--plan` before reading data (1–2
  times). Arm A computed only with cdors: the pilot used Python once to print JSON and `bc` to check a unit
  conversion, and T3-A's one "cdo" call was `which cdo`.
- **Speed.** Arm A was faster on four tasks: T1 6.2× (arm B's cdo run on the decade hit the agent's own 500 s
  timeout, and it switched to a zarr/numpy script), T2 1.5×, T3 2.1×, T5 1.9×. On T4, arm B was faster (65 s
  against 96 s): it wrote one numpy/healpy script, while arm A first read the documentation.
- **Cost.** Arm A used 1.7× more tool calls and 2.7× the tokens (2.42M against 0.91M; $2.73 against $1.57). Every
  arm-A session read the README and `docs/deviations.md` (34 kB) before working, and that text stays in the
  context of every later turn as cache reads. Agents know cdo and xarray from training; cdors they learn on the
  spot. A shorter agent-facing guide, or an MCP layer that describes the operators, would cut most of this.
- **The pilot's estimate** was 0.4–0.9M fresh tokens and 2.5–9M cache reads for the nine sessions after it; they
  used 0.32M fresh and 2.7M cache reads ($3.88).

## Findings beyond the scores

- **The T3 reference was wrong, and both arms found the right value.** The prompt asks for a 1° grid with cell
  centres at 0.5, 1.5, …, 359.5 E. The reference used cdo's `r360x180`, whose first longitude is 0°, and gave
  17.973827. Both arms built the prompt's grid themselves and answered 17.979334 (cdors) and 17.9793339 (cdo);
  cdo on that grid gives 17.979334. The reference is now 17.979334 (`bench/agent_tasks.md`, `ref_cdo.sh`); the
  old value is kept as a variant. No verdict changed: the old value is 3.1e-4 off, inside the tolerance.
  cdors and cdo agree to 8 digits on the same grid.
- **Scorer fixes.** Two patterns in `bench/agent/agent_score.py` were wrong.
  - Three arm-B sessions were flagged as possible peeks because they ran `cd <own session dir>; …`; the path
    pattern took the `;` into the directory name.
  - "cdors calls" counted every command mentioning the session path (`cdors-agentcheck/…`), so arm B seemed to
    use cdors.

  After the fixes no session is flagged except the pilot. Its flag comes from the location of my binary copy
  (under `cdors-agentcheck/`), not from the agent.
- T3-A read the first 3 kB of its own `transcript.jsonl` in the session directory. That file holds only its own
  prompt and settings, so it gained nothing from it, but the runner could keep the transcript outside the
  session directory.
- **Files in your home.** Despite `--no-session-persistence`, every session created a directory under
  `~/.claude/projects/-scratch-a-a270088-cdors-agentcheck-*`. Two of them hold a saved oversized tool output
  (35 and 37 kB); the other eight are empty. They are listed in `docs/STATUS.md` for cleanup.

## What it says about the prototype goal

"Agents use it reliably": in this check, yes. Five of five tasks were answered correctly with cdors alone, with no
failed commands. The agents checked the plan before reading data and needed less wall time than with the tools
they already know. The price is the learning cost in tokens, which a compact agent-facing reference would
reduce. The sample is small: one session per task and arm, with one model.
