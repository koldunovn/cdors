//! The model registry: one table, [`MODELS`], mapping each model to its loader, its rules and
//! its native capabilities.
//!
//! Model code lives only in `models/<name>/`, and no model name appears outside it: the rest of
//! cdors sees a model through its [`Model`] entry (how a mesh spec names it, which files its
//! loader reads, how the dimensions of its output files map to places and levels). Models do not
//! depend on each other. Adding a model is a directory plus one line in [`MODELS`].
//!
//! A **mesh spec** names a model mesh: `<prefix>:<dir>[,<option>=<value>...]`, e.g.
//! `fesom:/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/pi,units=rad` ([`parse_spec`]).

pub mod fesom2;

use crate::{ArrayReader, Mesh, MeshError, Result, WetLevels};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

/// What a dimension of a model's output files is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimRule {
    /// The cells of a place (`nod2` → `nodes`).
    Place(&'static str),
    /// Mid-levels (layers): FESOM2 `nz1`.
    Mid,
    /// Level interfaces: FESOM2 `nz`.
    Interfaces,
}

impl DimRule {
    pub fn name(self) -> &'static str {
        match self {
            Self::Place(p) => p,
            Self::Mid => "mid-levels",
            Self::Interfaces => "interfaces",
        }
    }
}

/// A model's loader: the mesh from readers over its files.
pub type LoadFn = fn(&[Arc<dyn ArrayReader>], &MeshSpec) -> Result<Mesh>;

/// One model: how specs name it, what its loader reads, its rules and capabilities.
pub struct Model {
    /// Name in messages, `mesh info` and [`crate::Source::model`] (`fesom2`).
    pub name: &'static str,
    /// The spec prefix (`fesom` in `fesom:<dir>`).
    pub prefix: &'static str,
    /// Files of the mesh directory the loader reads, in the order it takes them.
    pub files: &'static [&'static str],
    /// Spec options the loader understands, each with its allowed values (`units`: `deg`,
    /// `rad`, ...); [`parse_spec`] refuses other names and values.
    pub options: &'static [(&'static str, &'static [&'static str])],
    /// Builds the mesh from readers over [`Model::files`].
    pub load: LoadFn,
    /// Version of the loader: part of the mesh cache key, raised whenever the loader changes
    /// what it builds.
    pub version: u32,
    /// Dimension names of the model's output files and what they are.
    pub dims: &'static [(&'static str, DimRule)],
    /// What the mesh provides natively (for `mesh info` and operators that need it).
    pub native: &'static [&'static str],
}

/// All models.
pub static MODELS: &[&Model] = &[&fesom2::MODEL];

/// The model named `name` ([`Model::name`]).
pub fn model(name: &str) -> Option<&'static Model> {
    MODELS.iter().copied().find(|m| m.name == name)
}

/// A parsed mesh spec.
#[derive(Debug, Clone)]
pub struct MeshSpec {
    pub model: &'static Model,
    pub dir: PathBuf,
    /// `(option, value)` in the order given; only names of [`Model::options`].
    pub options: Vec<(String, String)>,
}

impl MeshSpec {
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The source files of the mesh (what the loader reads).
    pub fn files(&self) -> Vec<PathBuf> {
        self.model.files.iter().map(|f| self.dir.join(f)).collect()
    }

    /// The spec as text (options in the given order).
    pub fn text(&self) -> String {
        let mut s = format!("{}:{}", self.model.prefix, self.dir.display());
        for (k, v) in &self.options {
            s.push_str(&format!(",{k}={v}"));
        }
        s
    }
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model({})", self.name)
    }
}

/// Parses `<prefix>:<dir>[,<option>=<value>...]`. `None` if `spec` does not start with the
/// prefix of a model (it is then a file or a grid name); an error for unknown options.
pub fn parse_spec(spec: &str) -> Option<Result<MeshSpec>> {
    let (prefix, rest) = spec.split_once(':')?;
    let model = MODELS.iter().copied().find(|m| m.prefix == prefix)?;
    let mut parts = rest.split(',');
    let dir = parts.next().unwrap_or("");
    let mut options = Vec::new();
    for p in parts {
        let Some((k, v)) = p.split_once('=') else {
            return Some(Err(MeshError::Invalid(format!(
                "mesh spec '{spec}': '{p}' is not <option>=<value>"
            ))));
        };
        let Some((_, allowed)) = model.options.iter().find(|(name, _)| *name == k) else {
            let known: Vec<&str> = model.options.iter().map(|o| o.0).collect();
            return Some(Err(MeshError::Invalid(format!(
                "mesh spec '{spec}': unknown option '{k}' (known: {})",
                known.join(", ")
            ))));
        };
        if !allowed.contains(&v) {
            return Some(Err(MeshError::Invalid(format!(
                "mesh spec '{spec}': {k}={v} is not allowed ({k}={})",
                allowed.join(" | ")
            ))));
        }
        options.push((k.to_owned(), v.to_owned()));
    }
    if dir.is_empty() {
        return Some(Err(MeshError::Invalid(format!(
            "mesh spec '{spec}': no mesh directory"
        ))));
    }
    Some(Ok(MeshSpec {
        model,
        dir: PathBuf::from(dir),
        options,
    }))
}

/// What dimension `dim` of a data file on `mesh` is, by the rules of the mesh's model (`None`
/// for grids without a model and for dimensions the model does not name).
pub fn dim_rule(mesh: &Mesh, dim: &str) -> Option<DimRule> {
    let m = model(&mesh.source.model)?;
    m.dims.iter().find(|(d, _)| *d == dim).map(|&(_, r)| r)
}

/// The wet indices of `cell` along a vertical dimension: mid-levels `top..bottom`, interfaces
/// `top..bottom + 1`. Values outside are missing whatever the file stores there.
pub fn wet_range(wet: &WetLevels, rule: DimRule, cell: usize) -> Range<usize> {
    match rule {
        DimRule::Interfaces => wet.interfaces(cell),
        _ => wet.range(cell),
    }
}
