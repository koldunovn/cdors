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

### Step 2: the trap set is ready to run (2026-10-19)

The four candidates and a fifth task on ICON form `TASKSET=trap` (`bench/agent_tasks.md`, "Trap task set"):
- **W6 ICON:** the mean 2 m temperature south of 60° S of 1 January 1991, on the native R2B8 grid of the raw
  erc2002 output (no coordinates in the file; the grid file has `clon`/`clat` in radians and `cell_area`).
  Reference 265.801638 K, cdors `-fldmean -sellonlatbox,-180,180,-90,-60 -setgrid,<grid file>` 265.80164.
  Naive answers: cos(lat) +1.0 %, no weights −1.0e-3 (cell areas vary 1.43× there; the global mean would miss
  by only 1.4e-5), the second time step +2.7e-4.
- **Prompts:** they state the quantity and leave the storage conventions to the agent (unlike F1–F5).
- **Tolerances:** rel 1e-4 for all five tasks, which admits the accepted alternatives (≤ 2.2e-6) and rejects
  the nearest naive answer (≥ 2.7e-4).
- **Arm B environment:** nereus for W1, W2, W4 and W6; ecmwf for W5.
- **Dry run and scoring:** the dry run prints all ten prompts. The scorer marks the naive answers as their
  variants.

### Trap set run (job 28256601, 2026-10-19): the traps did not catch arm B

Ten of ten answers were correct (`bench/agent/results-20261019-215652.md`, model claude-opus-5-5, 17 min, 0.16 node-h,
$3.33).

| Task | A (cdors): calls, s, USD | B (Python/cdo): calls, s, USD |
|---|---|---|
| W1 | 5, 75, 0.20 | 3, 41, 0.19 |
| W2 | 7, 56, 0.26 | 7, 67, 0.42 |
| W4 | 15, 131, 0.60 | 4, 47, 0.20 |
| W5 | 11, 153, 0.46 | 8, 120, 0.44 |
| W6 | 8, 58, 0.33 | 5, 65, 0.22 |
| total | 46, 472, 1.85 | 27, 340, 1.48 |

cdors cost 1.25× as much as the Python arm (round 1's FESOM2 set: 1.10×) and took 1.4× the wall time. It was cheaper
only on W2.

- **Arm B avoided every naive answer.** It read `nod_area` with its `nl` dimension and took level k (W1). It cut the
  1950–2050 m layer at 2000 m (W2), after reading nereus' `volume_mean` source and computing its own sum instead.
  It used `elem_area` with vertex-mean centres (W4), and Gauss–Legendre weights divided by `pl` (W5). It even
  found the raw GRIB file and cross-checked with cdo through a hand-written reduced-Gaussian grid description.
  For W6 it used `cell_area`, cross-checked with cdo `-setgrid`. The step-1 naive variants are what a careless
  method gives, not what this model does.
- **Arm A had the right number early and then spent calls checking how cdors got it.** W4: the answer came at
  call 6 of 15. The remaining calls tried to find out how cdors places an element in a box (`ops --json`, `strings`
  on the binary, `toelems` on the mesh coordinates, boxes around the edges). W5: the answer came at call 8 of 11,
  then zonal bands to test the reduced-Gaussian weights. W6: the answer came at call 6 of 8, then
  `-setgridarea,cell_area` and a manual Σ T·A / Σ A to confirm that cdors used `cell_area`. The documentation does
  not say which weights or which membership rule a mean used, so a careful agent checks.
- **What this means for cdors:** the advantage of "right by default" is lost if the default cannot be seen. A
  result that states its method would save these calls: the weights' source and the membership rule, in `--json`
  output and in `--plan`. Examples: `per-level nod_area, dry layers excluded`; `reduced Gaussian row areas`;
  `cell_area from the grid file`; `elements by the centre of their three vertices`.

### Results that state their method, and a weaker model (jobs 28270068 and 28270070, 2026-10-20)

cdors 38ec395 adds a `method` list to `--json` results and `--plan`. It records the weights and their source, the
layers cut at a depth limit, the cells a box kept and how their centres are defined, the mesh's centres and areas,
and the grid file's coordinates. Job 28270068 reran arm A only, with the default model (Opus 5.5). Job 28270070
ran both arms with Sonnet 5.5, job 28273732 with Haiku 5.5. All 25 answers were correct
(`bench/agent/results-20261020-104003.md`, `results-20261020-105123.md`, `results-20261020-122858.md`; 10, 8
and 8 min).

| Task | Opus A, old (32b1dbd) | Opus A, `method` (38ec395) | Opus B (job 28256601) | Sonnet A | Sonnet B | Haiku A | Haiku B |
|---|---|---|---|---|---|---|---|
| W1 | 5, 75, 0.20 | 5, 77, 0.21 | 3, 41, 0.19 | 2, 55, 0.07 | 2, 66, 0.07 | 5, 55, 0.006 | 4, 29, 0.006 |
| W2 | 7, 56, 0.26 | 8, 64, 0.30 | 7, 67, 0.42 | 3, 53, 0.07 | 2, 24, 0.07 | 5, 49, 0.007 | 5, 66, 0.011 |
| W4 | 15, 131, 0.60 | 15, 191, 0.70 | 4, 47, 0.20 | 3, 38, 0.07 | 2, 21, 0.06 | 6, 43, 0.008 | 3, 31, 0.006 |
| W5 | 11, 153, 0.46 | 6, 42, 0.22 | 8, 120, 0.44 | 2, 17, 0.06 | 2, 51, 0.05 | 4, 24, 0.005 | 3, 59, 0.007 |
| W6 | 8, 58, 0.33 | 5, 35, 0.23 | 5, 65, 0.22 | 2, 16, 0.06 | 2, 20, 0.05 | 4, 22, 0.006 | 2, 68, 0.004 |
| total | 46, 472, 1.85 | 39, 409, 1.68 | 27, 340, 1.48 | 12, 178, 0.33 | 10, 182, 0.31 | 24, 193, 0.032 | 17, 253, 0.033 |

Each cell gives tool calls, wall seconds and USD.

- **The `method` list removed the checks on W5 and W6.** Calls fell from 11 to 6 and from 8 to 5; cost fell
  from $0.46 to $0.22 and from $0.33 to $0.23. Arm A ran the chain, then checked only min/max and
  missing values. Opus arm A now costs 1.14× its arm B from job 28256601. Without W4 it costs 0.77×
  ($0.98 vs $1.27).
- **W4 is unchanged.** Arm A had 0.02787067 at call 5. It spent the other 10 calls on elements whose centres lie
  within a few metres of the box edges. It shifted the box by 1e-3° and 2e-5°, and listed those elements. It
  rebuilt their centres from the vertices with awk to see whether the result depends on the definition of the
  centre. The note says how the centres are defined, not how many cells sit on an edge, so this check is a
  sensitivity test the note does not answer. A count of the kept cells whose centres lie within a small
  distance of an edge would answer it; that is not implemented.
- **The traps did not catch Sonnet either.** Arm B wrote one script per task. It used per-level `nod_area` by
  `nlevels_nod2D`, cut the 1950–2050 m layer with a clip, and used `elem_area` with vertex-mean centres. It used
  Gauss–Legendre weights divided by the points of each row, and `cell_area` from the ICON grid. W1 came closest
  to a miss. Arm B printed the top, bottom and mean interface areas (the bottom one is 4.3e-4 off) and chose the
  top area "because I believe FESOM2 uses it". It said it had not confirmed this.
- **Nor Haiku.** Arm B took `nod_area[50]` with `nlevels_nod2D` (W1). It checked that the nonzero `nod_area`
  rows match the wet layers and clipped the layers to 1000–2000 m (W2). It used Gauss weights shared among the
  `pl` points of each row, after matching the row latitudes to the Gauss–Legendre nodes (W5). The arms cost the
  same ($0.032 vs $0.033); Haiku arm A used more calls (24 vs 17) and less wall time (193 s vs 253 s). A Haiku
  session costs about a tenth of a Sonnet session.
- **With Sonnet the arms cost the same:** arm A $0.33 against arm B $0.31 (1.07×), with the same wall time.
  Sonnet sessions cost about a fifth of Opus sessions. Sonnet arm A ran the cdors chain and answered, without
  the extra checks.
- **Conclusion:** the five trap tasks test conventions that both models know: FESOM2 per-level areas, reduced
  Gaussian rows and ICON `cell_area`. None of the three models makes the naive mistakes. When an answer takes one Python
  script, cdors can save only the script, and the arms cost about the same. A difference in cost needs tasks
  where the script is long: transports through a section and the MOC, which nereus does not provide.

### A long-script task: the AMOC from w (job 28288688, 2026-10-20)

H1 asks for the AMOC at 26.5 N in January 1990 on dart: the maximum over depth of the northward transport above
each interface, from w by continuity, in the Atlantic_MOC basin of nereus (`bench/agent_tasks.md`, "Long-script
task set"). In cdors this is one operator, `moc` (3ffcfbb). Without cdors it takes a script: nereus has no MOC.
The reference, 17.4749385 Sv at 950 m, is in job 28281059. cdors agrees with it to 1e-12 at every interface.
The naive answers are 19–46 % off. One Opus 5.5 session per arm, frozen binary `cdors-3ffcfbb`:

| Arm | Answer | Calls | Wall s | USD |
|---|---|---|---|---|
| A (cdors) | 17.4749385 Sv, 950 m | 9 | 74 | 0.36 |
| B (Python, nereus) | 17.3185 Sv, 950 m | 15 | 303 | 0.87 |

- **Arm A cost 0.42× arm B,** in a quarter of the wall time. This is the first task where the cost differs
  clearly. Arm A read the guide and `cdors help moc`, planned, ran the chain once and took the maximum with jq.
- **Arm B was correct, and the first verdict was not.** It used w · `nod_area[k]` over the basin's nodes north of
  the line, summed from the north. Then it removed the surface term, −∫w(0) dA = 0.216 Sv, because the prompt
  defines psi as the transport between the surface and z, so psi(0) = 0. cdors and pyfesom2's `xmoc_data`
  compute −∫w(z) dA, which is minus the transport below z. With the linear free surface the two differ by the
  net surface flux north of the line. The prompt allows both readings, so the tolerance was widened from
  0.09 Sv to 0.25 Sv after the run. That changes the verdict from fail to correct
  (`bench/agent/results-20261020-204205-rescored.md`; the first scoring is kept in `results-20261020-204205.md`).
  Arm A described cdors' sum as "the northward transport above" the interface. It did not notice that psi(0) =
  0.22 Sv contradicts that.
- **Arm B read pyfesom2's source** (`xmoc_data`, from the user's home directory; pyfesom2 itself was not on its
  PATH) to pick the direction of the sum. The scorer does not flag this, because it is not our reference. It
  spent two runs on node coordinates stored in radians.
- **What this says for cdors:** the method note, the help and the guide now say which transport psi is: minus
  the transport below the interface, so psi(0) = −∫w(0) dA is not 0. They also give the other reading, the
  output minus its top interface (`-sub moc.nc -sellevel,0 moc.nc`). An option in `moc` itself would need
  the surface interface in every batch of fields, so there is none. One session per arm is a single sample;
  a repeat costs about $1.2.

### The Drake Passage transport (job 28291911, 2026-10-20)

H2 asks for the volume transport through Drake Passage in January 1990 on dart. The section is the great circle
from 68 W 54 S to 63 W 65.5 S, and the transport is the integral of the normal velocity, positive eastward
(`bench/agent_tasks.md`, "H2"). In cdors it is one operator, `transport` (8e48256). The reference is
161.191441 Sv, from job 28291761, and cdors agrees with it to 2e-16. Velocities rotated a second time give
+13.5 % and u alone −20.5 %. One Opus 5.5 session per arm, frozen binary `cdors-8e48256`:

| Arm | Answer | Calls | Wall s | USD |
|---|---|---|---|---|
| A (cdors) | 161.191441 Sv | 7 | 90 | 0.31 |
| B (Python, nereus) | 161.181 Sv (−6.5e-5) | 8 | 233 | 0.55 |

- **Arm A cost 0.57× arm B.** Over H1 and H2 together the cdors arm cost 0.48× ($0.67 against $1.41) and took 0.31× the
  wall time (164 s against 536 s).
- **Arm A** read the guide and `cdors help transport`, planned, and ran the chain once with `-P 16`. It
  checked that the velocities are geographic (`FESOM_autorotate_back_to_geo`) and that there was no
  `section_open_ends` warning.
- **Arm B** wrote one 150-line script and one check, and did not look at pyfesom2 this time. It cut the section
  into its pieces inside each wet triangle, which is the exact chord, not the dual-cell faces. It took the pole
  of the great circle as the normal, `nlevels − 1` layers and `r_earth`, and checked the radius against
  `elem_area`. It took u and v as geographic without checking. The rotation trap only catches an agent that
  copies pyfesom2's code with `uvrotated=True`.
- **Both arm A sessions** (H1 and H2) spent a call on threads. The jobs ran on the shared partition with
  `--mem=64G`, which Slurm turns into 70 cores (940 MB per core), and cdors planned 140 or 108 threads because
  it counts SMT siblings. The agents set `-P 16` themselves.
