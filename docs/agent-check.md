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

## Re-check with `cdors guide` (2026-10-10)

The cdors arm again: the same five tasks and settings, but with `cdors guide` (5.4 kB, `docs/agent-guide.md`)
instead of copies of the README and `docs/deviations.md` (34 kB). Run `20261010-005426`
(`DOCS=guide bench/agent_check.sh`), cdors build 173f12a, results in `bench/agent/results-20261010-005426.md`.

| | cdors, README + deviations | cdors, `cdors guide` | cdo + Python |
|---|---|---|---|
| Correct | 5 of 5 | 5 of 5 | 5 of 5 |
| Wall time | 704 s | 426 s | 1684 s |
| Tool calls | 69 | 56 | 40 |
| Tokens (fresh + cache reads) | 0.24M + 2.19M | 0.17M + 1.42M | 0.13M + 0.78M |
| Cost at list price | $2.73 | $1.98 | $1.57 |

Per task: T1 56 s ($0.27), T2 149 s ($0.52), T3 88 s ($0.45), T4 90 s ($0.48), T5 44 s ($0.26). No session was
flagged. With the guide, the cdors arm cost 27% less and took 39% less time: 1.3× the cost of cdo + Python, and
4× faster.

## FESOM2 set (round 1, 2026-10-19)

**Ten of ten correct; with cdors half the wall time (838 s against 1697 s) at 1.10× the cost ($2.24 against
$2.03).** Run `20261019-140945` (job 28244863, shared partition, 44 min), frozen binary `cdors-32b1dbd`, model
claude-opus-5-5, results in `bench/agent/results-20261019-140945.md`:

| | cdors, `cdors guide` | Python + nereus, cdo |
|---|---|---|
| Correct | 5 of 5 | 5 of 5 |
| Wall time | 838 s | 1697 s |
| Turns / tool calls | 57 / 52 | 59 / 54 |
| Tokens (all, incl. cache reads) / output | 1.68M / 30k | 1.46M / 41k |
| Cost at list price | $2.24 | $2.03 |

Per task, cdors against Python: F1 $0.45 against $0.55 (109 s against 721 s), F2 $0.40 against $0.76 (187 s
against 545 s), F3 $0.65 against $0.23, F4 $0.36 against $0.25, F5 $0.38 against $0.24.
- **F1 and F2 (mesh-weighted series):** cdors is cheaper; the Python arm built the area and depth weights and the
  masks by hand (40 turns in all).
- **F3–F5:** one NumPy script answers each in 6–7 turns, while every cdors session first spends 5–6 calls on the
  guide, the help of two to four operators, `sinfo`, `mesh info` and `--plan`.
- **Threads:** all five cdors sessions spent a call looking them up, because the guide does not say that inside a
  Slurm job the defaults already use the job's cores.
- **F3 (16 turns):**
  - `outputtab,station_name` of the written section file failed twice (cdors reads no string variables from stored
    files; the hint says so).
  - `-vertint -addc,1 -mulc,0` gave the full column depth instead of the wet one. As in cdo, zero times missing is
    zero.
  - The session checked against `bottom_depth` and corrected it.
- **Spread:** the pilot (job 28236330, `results-20261019-082207.md`) ran F5 with cdors in 9 turns for $0.33, against
  11 turns and $0.38 in the full run.

### Setup

Five tasks on raw FESOM2 output (AWI-CM3 `hist1950d`, dart mesh, 3.16M nodes): F1 Atlantic heat content 0–700 m
per month of 1990–1991, F2 the NH ice-extent seasonal cycle from daily concentration (1990–1991), F3 a Drake
Passage section of temperature (65° W, January 2000), F4 the NINO3.4 anomalies of 2005 (base 2001–2005), F5 the
Arctic (> 66.5° N) mean SST per year 2006–2010. Arm A: cdors with `cdors guide` only (`DOCS=guide`); arm B: cdo
2.6.0 and Python with nereus 0.4.1 (env `nereus`). Both arms get the mesh directory in the prompt.

Prompts, references, tolerances, variants and the cdors cross-check are in `bench/agent_tasks.md` (section
"FESOM2 task set"). The references come from NumPy scripts (`bench/agent/ref_fesom_*.py`) with nereus as the
second method for F1 and F2. They were computed on a compute node (job 28230786, `bench/agent/ref_fesom.sh`,
4 min 50 s). The cdors commands an expert would use agree with them: to 1e-14 relative on F1–F3, and to 1e-6 °C
on F4–F5, where cdors writes float32. Three cdors findings came up along the way; they are listed there.
The largest: `maskregion,regions=…` inflates the planned bytes 5×, so on a login node the natural F1 command is
refused by the 64 GB read limit.

Run: `TASKSET=fesom bench/agent_check.sh` (dry run); on a compute node `TASKSET=fesom CDORS_BIN=<binary> sbatch
--time=02:30:00 bench/agent_check.sbatch` (add `TASKS=F5 ARMS=A` and `--time=00:40:00` for a pilot). Estimated
beforehand at $4–6 for the ten sessions and 40–90 min; it took $4.27 and 44 min.

## Trap candidates for the next set (step 1, 2026-10-19)

Ten of ten correct answers in both arms measure only cost. The next set should have tasks where an agent without
cdors gets a plausible but wrong number (Nikolay, 2026-10-19). Step 1 checked four candidates. For each one,
`bench/agent/ref_trap.py` computes, independent of cdors, the reference (careful NumPy, float64) and the answers of
the naive methods an agent is likely to use. A task is kept only if those naive answers miss the tolerance and the
chain an agent would write with cdors gives the reference. **All four pass**: the naive methods miss by 0.04–39 %,
and cdors agrees with every reference to 3e-7 or better.

Job 28250598 (shared partition, 32 cores, 14.5 min, `bench/agent/ref_trap.sh`, outputs in
`/scratch/a/a270088/cdors-bench/trap-refs-28250598/`). W5's reference was computed on the login node in env
`ecmwf` (`W5_ecmwf.json` there): only that env's eccodes 2.34 decodes these GRIB messages; envs `nereus` (2.44) and
`hk25` (2.41) fail with "Key/value not found".

| | Task (dart, January 1990; W5 IFS, January 1950) | Reference | cdors chain | Naive answers |
|---|---|---|---|---|
| W1 | global mean temperature at the 1600 m level | 2.752827 °C | `-fldmean -sellevel,1600` 2.7528267 | no weights −39 %; surface areas with the zeros below the floor −13 %; no weights, zeros dropped −9.3 %; surface areas, zeros dropped +0.06 % |
| W2 | volume-mean temperature of the water between 1000 and 2000 m | 3.083167 °C | `-volmean,1000/2000` 3.083167 | nereus `volume_mean(…, mesh["area"], …, depth_min=1000, depth_max=2000)` −13.9 %; the same with 2-D `nod_area` −1.3 % (whole layers by mid-depth: 1950–2050 m counted whole); layers without thickness +3.2 %; whole layers inside +1.4 %; cut, surface areas, wet only +0.04 % |
| W4 | mean surface zonal velocity (on elements) over the Agulhas box 10–40° E, 45–30° S | 0.0278707 m/s | `-fldmean -sellonlatbox,10,40,-45,-30 -sellevel,2.5` 0.02787067 | cos(lat) weights −14 %; no weights +9.4 %; elements with all three vertices in the box −0.8 % |
| W5 | global mean 2 m temperature on the native O1280 grid (gribscan references) | 284.89708 K | `-fldmean -selname,mean2t` 284.89715 | cos(lat) per point +1.8 % (+5.1 K); no weights +0.56 % (+1.6 K), also what cdo gives on reduced Gaussian grids |

Notes:
- **Dry levels:** in dart they are stored as exactly 0 (1 050 345 of 3 160 340 nodes at 1600 m), so anything that
  does not use the per-level areas (`nod_area[k]`, 0 below the floor) or the wet levels averages zeros in.
- **W4 boxes:** element areas vary 28–137 km² in the Agulhas box. Of the five boxes tried, it gives the largest
  naive errors. In the Gulf Stream box no weights miss by −1.7 %, in the Nordic Seas cos(lat) misses by +16 %; in
  Drake Passage and Niño 3.4 the errors are below 4 %.
- **W5:** the reference weights each point by its row's Gaussian quadrature weight divided by the points in the
  row (rows match the Gauss–Legendre latitudes to 1e-12°). cdors uses the area between the rows' latitude bounds,
  2.5e-7 apart. The `cdo` in arm B cannot read these messages (2.6.0); arm B needs the `ecmwf` env to decode them.
- **Partial bottom cells:** dart has them (`zbar_n_bottom` equals a `zbar` interface at only 4 % of the nodes). Like
  nereus, cdors uses the full `zbar` layers (`docs/deviations.md`), so W2's prompt must say so.
- **Dropped:** a day-weighted annual mean from monthly means, because cdors lacks `yearmonmean`.

Gaps in cdors found on the way:
- `yearmonmean` is not implemented.
- A reduced Gaussian field written by cdors to NetCDF loses its rows: read back, it is unstructured, and `fldmean`
  falls back to equal weights with the loud warning (286.484 K instead of 284.897 K), although the file keeps
  `gridType = "reduced_gg"` and `N = 1280`.
