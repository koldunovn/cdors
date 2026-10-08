//! CDO's own operator catalog: every cdo operator with its section, arity and one-line
//! description, and the help text `cdo -h <operator>` prints.
//!
//! The data is extracted from the CDO 2.6.5 source by `tools/gen_catalog.py` into
//! `crates/cdors-core/data/` (see the README there; CDO is BSD-3-Clause, the notice is kept in
//! each data file) and embedded at compile time. It serves `cdors ops` (cdo operators that cdors
//! does not implement, by name), `cdors help <op>` and did-you-mean hints.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::LazyLock;

const OPERATORS_TSV: &str = include_str!("../../data/cdo_operators.tsv");
const ALIASES_TSV: &str = include_str!("../../data/cdo_aliases.tsv");
const HELP_TXT: &str = include_str!("../../data/cdo_help.txt");
/// Starts the line naming a help text in `cdo_help.txt`.
const HELP_MARK: &str = "%%% ";

/// One operator of CDO's `OPERATORS` catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CdoOperator {
    pub name: &'static str,
    /// Section of the catalog, e.g. `Statistic`, `Interpolation`.
    pub section: &'static str,
    /// Documentation module (operator family), e.g. `Yearstat`.
    pub module: &'static str,
    /// Number of input streams; -1 means any number (e.g. `cat`, `ensmean`).
    pub n_inputs: i8,
    /// Number of output streams; -1 means several files named from an output base name
    /// (e.g. `splityear`).
    pub n_outputs: i8,
    /// One-line description from the catalog.
    pub description: &'static str,
    /// Key of the help text in `cdo_help.txt` (empty if cdo has none).
    #[serde(skip)]
    help_key: &'static str,
}

impl CdoOperator {
    /// The help text `cdo -h <name>` prints for this operator, if cdo has one.
    pub fn help(&self) -> Option<&'static str> {
        CATALOG.help.get(self.help_key).copied()
    }
}

struct Catalog {
    operators: Vec<CdoOperator>,
    by_name: HashMap<&'static str, usize>,
    aliases: HashMap<&'static str, &'static str>,
    help: HashMap<&'static str, &'static str>,
}

/// Data rows of a generated TSV file (comment lines start with `#`).
fn rows(tsv: &'static str) -> impl Iterator<Item = Vec<&'static str>> {
    tsv.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split('\t').collect())
}

fn parse_help(text: &'static str) -> HashMap<&'static str, &'static str> {
    let starts: Vec<usize> = text
        .match_indices(&format!("\n{HELP_MARK}"))
        .map(|(i, _)| i + 1 + HELP_MARK.len())
        .collect();
    let mut help = HashMap::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts
            .get(k + 1)
            .map_or(text.len(), |&n| n - HELP_MARK.len());
        let (key, body) = text[s..end]
            .split_once('\n')
            .expect("cdo_help.txt: key line");
        help.insert(key, body);
    }
    help
}

fn arity(s: &str) -> i8 {
    s.parse().expect("cdo_operators.tsv: arity")
}

fn build() -> Catalog {
    let operators: Vec<CdoOperator> = rows(OPERATORS_TSV)
        .map(|f| {
            assert_eq!(f.len(), 7, "cdo_operators.tsv: 7 columns expected");
            CdoOperator {
                name: f[0],
                section: f[1],
                module: f[2],
                n_inputs: arity(f[3]),
                n_outputs: arity(f[4]),
                help_key: f[5],
                description: f[6],
            }
        })
        .collect();
    let by_name = operators
        .iter()
        .enumerate()
        .map(|(i, o)| (o.name, i))
        .collect();
    let aliases = rows(ALIASES_TSV).map(|f| (f[0], f[1])).collect();
    Catalog {
        operators,
        by_name,
        aliases,
        help: parse_help(HELP_TXT),
    }
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(build);

/// All operators of CDO's catalog, in catalog order (section by section).
pub fn cdo_operators() -> &'static [CdoOperator] {
    &CATALOG.operators
}

/// A cdo operator by name or by one of cdo's aliases (`chvar` → `chname`).
pub fn cdo_operator(name: &str) -> Option<&'static CdoOperator> {
    let name = CATALOG.aliases.get(name).copied().unwrap_or(name);
    CATALOG.by_name.get(name).map(|&i| &CATALOG.operators[i])
}

/// The help text `cdo -h <name>` prints (cdo 2.6.5 source; `cdo -h` adds one more newline).
/// Aliases resolve to their operator, where cdo itself aborts.
pub fn cdo_help(name: &str) -> Option<&'static str> {
    cdo_operator(name).and_then(CdoOperator::help)
}

/// cdo's aliases as (alias, operator) pairs.
pub fn cdo_aliases() -> impl Iterator<Item = (&'static str, &'static str)> {
    CATALOG.aliases.iter().map(|(&a, &o)| (a, o))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1)
                .min(cur[j] + 1)
                .min(prev[j] + usize::from(ca != cb));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Did-you-mean candidates for an unknown operator name: cdo operators, cdo aliases and the
/// names in `implemented` within edit distance 2 of `name` (case-insensitive), excluding `name`
/// itself. Implemented names come first, then by distance and name; at most five.
pub fn suggest<'a>(name: &str, implemented: &[&'a str]) -> Vec<&'a str> {
    let lower = name.to_lowercase();
    let mut scored: Vec<(bool, usize, &'a str)> = implemented
        .iter()
        .copied()
        .chain(CATALOG.operators.iter().map(|o| o.name))
        .chain(CATALOG.aliases.keys().copied())
        .filter(|&n| n != name)
        .map(|n| {
            let d = levenshtein(&lower, &n.to_lowercase());
            (!implemented.contains(&n), d, n)
        })
        .filter(|&(_, d, _)| d <= 2)
        .collect();
    scored.sort_unstable();
    scored.dedup_by_key(|s| s.2);
    scored.into_iter().take(5).map(|(_, _, n)| n).collect()
}
