//! Vertical axes.

use serde::Serialize;

/// A vertical axis: level values from the coordinate variable, with optional layer bounds.
#[derive(Debug, Clone, Serialize)]
pub struct ZAxis {
    /// Coordinate variable name.
    pub var: String,
    /// Dimension name.
    pub dim: String,
    pub values: Vec<f64>,
    pub units: Option<String>,
    pub long_name: Option<String>,
    pub standard_name: Option<String>,
    /// CF `positive` attribute (`up`/`down`).
    pub positive: Option<String>,
    /// Layer bounds, two values per level, if a bounds variable exists.
    pub bounds: Option<Vec<[f64; 2]>>,
}

impl ZAxis {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
