//! Horizontal grids.
//!
//! Grid kinds follow CDI's netCDF reader (`libcdi/src/stream_cdf_i.c`):
//! - **regular** lon-lat: 1-D `lon(lon)` and `lat(lat)` coordinate variables;
//! - **Gaussian**: like regular, but the latitudes are not equidistant and match Gaussian
//!   latitudes (CDI's `latitudes_are_gaussian`); treated like regular by operators;
//! - **curvilinear**: 2-D `lon(y,x)`/`lat(y,x)` named in the variable's `coordinates` attribute;
//! - **unstructured**: 1-D `lon(cell)`/`lat(cell)` over one cell dimension (ICON's
//!   `clon`/`clat` in radians included), optional vertex bounds `(cell, nv)`;
//! - **HEALPix**: the variable's `grid_mapping` names a variable with
//!   `grid_mapping_name = "healpix"`, `healpix_nside` and `healpix_order` (`nest*` or `ring`),
//!   as in `libcdi/src/grid.c:898` and `stream_cdf_i.c:1129`;
//! - **generic**: horizontal dimensions without usable coordinates (e.g. FESOM's `nod2`).
//!
//! Coordinates of regular and Gaussian grids are small and loaded when the dataset is opened;
//! those of curvilinear and unstructured grids are only named here and read on demand through the
//! chunk source (`ChunkSource::read_var`).

use super::dataset::Attrs;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GridKind {
    Regular,
    Gaussian,
    Curvilinear,
    Unstructured,
    Healpix,
    Generic,
}

impl GridKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Gaussian => "gaussian",
            Self::Curvilinear => "curvilinear",
            Self::Unstructured => "unstructured",
            Self::Healpix => "healpix",
            Self::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealpixOrder {
    Nested,
    Ring,
}

/// HEALPix parameters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Healpix {
    pub nside: u64,
    pub order: HealpixOrder,
    /// Name of a cell-index coordinate (standard_name `healpix_index`), if the grid is a subset.
    pub index_var: Option<String>,
}

/// A grid-mapping variable (`grid_mapping` attribute of the data variable).
#[derive(Debug, Clone, PartialEq)]
pub struct GridMapping {
    /// Name of the grid-mapping variable.
    pub var: String,
    /// Its `grid_mapping_name`.
    pub name: String,
    /// All its attributes in file order.
    pub attrs: Attrs,
}

/// Description of one coordinate axis (x or y) of a grid.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CoordAxis {
    /// Name of the coordinate variable.
    pub var: String,
    /// Dimension name (for 2-D/1-D cell coordinates: the x/cell dimension).
    pub dim: String,
    pub long_name: Option<String>,
    pub units: Option<String>,
    pub standard_name: Option<String>,
    /// Coordinate variable stored as float32.
    pub is_f32: bool,
    /// Name of its bounds variable, if any.
    pub bounds_var: Option<String>,
}

/// A horizontal grid.
#[derive(Debug, Clone)]
pub struct Grid {
    pub kind: GridKind,
    /// Number of horizontal points.
    pub size: usize,
    /// Number of columns (regular, Gaussian, curvilinear, generic); `size` for
    /// unstructured and HEALPix.
    pub xsize: usize,
    /// Number of rows; 0 for unstructured and HEALPix grids.
    pub ysize: usize,
    /// Horizontal dimension names in storage order (`[lat, lon]`, `[y, x]` or `[cell]`).
    pub dims: Vec<String>,
    /// Longitude axis (absent for HEALPix and generic grids).
    pub x: Option<CoordAxis>,
    /// Latitude axis.
    pub y: Option<CoordAxis>,
    /// Longitudes of regular/Gaussian grids (loaded at open).
    pub xvals: Option<Vec<f64>>,
    /// Latitudes of regular/Gaussian grids (loaded at open).
    pub yvals: Option<Vec<f64>>,
    /// Coordinates in radians (ICON `clon`/`clat`).
    pub radians: bool,
    /// Number of vertices per cell (bounds), if bounds exist.
    pub nvertex: Option<usize>,
    /// Name of the vertex dimension of the bounds.
    pub vdim: Option<String>,
    /// Gaussian grids: number of latitudes between pole and equator.
    pub gaussian_np: Option<usize>,
    pub healpix: Option<Healpix>,
    pub mapping: Option<GridMapping>,
    /// ICON grid reference of unstructured grids (global attributes `number_of_grid_used`,
    /// `grid_file_uri`, `uuidOfHGrid`; variable attribute `number_of_grid_in_reference`).
    pub reference: Option<GridReference>,
}

/// Reference to an ICON grid file, as CDI reads it from unstructured-grid NetCDF output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridReference {
    pub number: Option<i64>,
    pub position: i64,
    pub uri: Option<String>,
    pub uuid: Option<String>,
}

impl Grid {
    /// An empty grid of the given kind; the builder fills in the rest.
    pub fn new(kind: GridKind, dims: Vec<String>, xsize: usize, ysize: usize) -> Self {
        Self {
            kind,
            size: xsize * ysize.max(1),
            xsize,
            ysize,
            dims,
            x: None,
            y: None,
            xvals: None,
            yvals: None,
            radians: false,
            nvertex: None,
            vdim: None,
            gaussian_np: None,
            healpix: None,
            mapping: None,
            reference: None,
        }
    }

    /// Whether two grids describe the same coordinates (used to deduplicate grids).
    pub fn same_as(&self, other: &Grid) -> bool {
        self.kind == other.kind
            && self.dims == other.dims
            && self.x.as_ref().map(|a| &a.var) == other.x.as_ref().map(|a| &a.var)
            && self.y.as_ref().map(|a| &a.var) == other.y.as_ref().map(|a| &a.var)
            && self.mapping.as_ref().map(|m| &m.var) == other.mapping.as_ref().map(|m| &m.var)
    }

    /// Whether the grid has horizontal coordinates (or, for HEALPix, an analytic definition).
    pub fn has_coordinates(&self) -> bool {
        self.kind != GridKind::Generic
    }
}

/// CDI's increment check (`grid_calc_increment`): the mean increment if all steps agree with it
/// within 1 %, otherwise 0.
pub fn calc_increment(vals: &[f64]) -> f64 {
    let n = vals.len();
    if n < 2 {
        return 0.0;
    }
    let inc = (vals[n - 1] - vals[0]) / (n - 1) as f64;
    let tol = 0.01 * inc.abs();
    for w in vals.windows(2) {
        if ((w[0] - w[1]).abs() - inc.abs()).abs() > tol {
            return 0.0;
        }
    }
    inc
}

/// Gaussian latitudes in degrees, north to south (roots of the Legendre polynomial of degree n).
pub fn gaussian_latitudes(n: usize) -> Vec<f64> {
    let mut lats = vec![0.0; n];
    let nf = n as f64;
    for i in 0..n.div_ceil(2) {
        let mut x = (std::f64::consts::PI * (i as f64 + 0.75) / (nf + 0.5)).cos();
        for _ in 0..100 {
            // Legendre recursion: p1 = P_n(x), p0 = P_{n-1}(x)
            let (mut p0, mut p1) = (1.0, x);
            for k in 2..=n {
                let kf = k as f64;
                let p2 = ((2.0 * kf - 1.0) * x * p1 - (kf - 1.0) * p0) / kf;
                p0 = p1;
                p1 = p2;
            }
            let dp = nf * (x * p1 - p0) / (x * x - 1.0);
            let dx = p1 / dp;
            x -= dx;
            if dx.abs() < 1e-15 {
                break;
            }
        }
        let lat = x.asin().to_degrees();
        lats[i] = lat;
        lats[n - 1 - i] = -lat;
    }
    lats
}

/// CDI's `latitudes_are_gaussian`: within (lat0 - lat1)/500 of the Gaussian latitudes, in either
/// direction.
pub fn latitudes_are_gaussian(lats: &[f64]) -> bool {
    let n = lats.len();
    if n <= 2 {
        return false;
    }
    let g = gaussian_latitudes(n);
    let tol = (g[0] - g[1]) / 500.0;
    let fwd = (0..n).all(|i| (g[i] - lats[i]).abs() <= tol);
    let bwd = (0..n).all(|i| (g[i] - lats[n - 1 - i]).abs() <= tol);
    fwd || bwd
}
