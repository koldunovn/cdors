# CDO operator catalog data

These files are extracted from the CDO 2.6.5 source tree and embedded into cdors by
`crates/cdors-core/src/ops/catalog.rs` (`include_str!`). They serve `cdors ops` (the cdo operators
cdors does not implement, by name), `cdors help <op>` and did-you-mean hints.

| File | Content | From |
|---|---|---|
| `cdo_operators.tsv` | name, section, module, number of input and output streams, help key, one-line description; 722 operators | `OPERATORS`, `src/operators/*.cc` (`CdoModule module = {...}`) |
| `cdo_aliases.tsv` | alias, operator (31 aliases) | `.aliases` of the module structs |
| `cdo_help.txt` | 222 help texts, each after a line `%%% <HelpName>`, formatted as `cdo -h <op>` prints them | `src/operator_help.cc`, formatted like `Modules::construct_help()` in `src/module_info.cc` (EXAMPLE and AUTHOR dropped, blank lines removed) |

Stream counts: `-1` inputs means any number of inputs; `-1` outputs means several output files
named from a base name (cdo's `OBASE`).

## Regenerating

```sh
/work/ab0995/a270088/mambaforge/bin/python -I tools/gen_catalog.py /work/ab0995/a270088/cdors-ref/cdo-2.6.5
cargo run --example catalog_check   # after `source env.sh`; compares cdo_help() with `$CDO -h <op>`
```

The script uses only the Python standard library, prints counts, and writes into this directory
(or into the directory given as its second argument). Help texts compared against cdo 2.6.0's
`cdo -h`: 346 of 722 operators byte-identical; the differences inspected are help-text changes
between 2.6.0 and 2.6.5 (new operators such as `zonint`, rewritten sections).

## Licence

CDO is Copyright 2002-2026, MPI für Meteorologie, and distributed under the BSD-3-Clause licence.
Each data file starts with CDO's copyright line and licence notice, as `#` comment lines, which
the redistribution terms require to be kept.
