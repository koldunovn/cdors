//! Dataset, variable, dimension and attribute descriptions (metadata only, no data values).

use super::grid::Grid;
use super::time::TimeAxis;
use super::zaxis::ZAxis;
use serde::Serialize;
use serde_json::Value;

/// An attribute value. Integers and floats are kept as vectors (CF attributes may be arrays).
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Text(String),
    Ints(Vec<i64>),
    /// Values stored as 32-bit floats (printed with 7 digits by CDO).
    F32s(Vec<f64>),
    F64s(Vec<f64>),
}

impl AttrValue {
    /// First numeric value as f64 (text: parsed if possible).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Text(s) => s.trim().parse().ok(),
            Self::Ints(v) => v.first().map(|&x| x as f64),
            Self::F32s(v) | Self::F64s(v) => v.first().copied(),
        }
    }

    /// All numeric values as f64.
    pub fn as_f64s(&self) -> Vec<f64> {
        match self {
            Self::Text(s) => s.trim().parse().ok().into_iter().collect(),
            Self::Ints(v) => v.iter().map(|&x| x as f64).collect(),
            Self::F32s(v) | Self::F64s(v) => v.clone(),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Converts a JSON attribute value (Zarr `.zattrs` / `attributes`).
    pub fn from_json(v: &Value) -> Self {
        match v {
            Value::String(s) => Self::Text(s.clone()),
            Value::Bool(b) => Self::Ints(vec![i64::from(*b)]),
            Value::Number(n) => match n.as_i64() {
                Some(i) => Self::Ints(vec![i]),
                None => Self::F64s(vec![n.as_f64().unwrap_or(f64::NAN)]),
            },
            Value::Array(a) if !a.is_empty() && a.iter().all(|x| x.is_i64()) => {
                Self::Ints(a.iter().filter_map(Value::as_i64).collect())
            }
            Value::Array(a) if !a.is_empty() && a.iter().all(|x| x.is_number()) => {
                Self::F64s(a.iter().filter_map(Value::as_f64).collect())
            }
            Value::Array(a) if a.iter().all(|x| x.is_string()) => Self::Text(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            other => Self::Text(other.to_string()),
        }
    }

    pub fn to_json(&self) -> Value {
        let one_or_many = |v: Vec<Value>| {
            if v.len() == 1 {
                v.into_iter().next().unwrap_or(Value::Null)
            } else {
                Value::Array(v)
            }
        };
        match self {
            Self::Text(s) => Value::from(s.clone()),
            Self::Ints(v) => one_or_many(v.iter().map(|&x| Value::from(x)).collect()),
            Self::F32s(v) | Self::F64s(v) => one_or_many(
                v.iter()
                    .map(|&x| serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number))
                    .collect(),
            ),
        }
    }
}

/// Attributes in file order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attrs(pub Vec<(String, AttrValue)>);

impl Attrs {
    pub fn get(&self, name: &str) -> Option<&AttrValue> {
        self.0.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(AttrValue::as_str)
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get(name).and_then(AttrValue::as_f64)
    }

    pub fn iter(&self) -> impl Iterator<Item = &(String, AttrValue)> {
        self.0.iter()
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect(),
        )
    }
}

/// Stored data type of a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DType {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// Strings, characters, compound or other types cdors does not compute on.
    Other,
}

impl DType {
    /// numpy-style name (`float32`, `int16`, ...).
    pub fn name(self) -> &'static str {
        match self {
            Self::I8 => "int8",
            Self::I16 => "int16",
            Self::I32 => "int32",
            Self::I64 => "int64",
            Self::U8 => "uint8",
            Self::U16 => "uint16",
            Self::U32 => "uint32",
            Self::U64 => "uint64",
            Self::F32 => "float32",
            Self::F64 => "float64",
            Self::Other => "other",
        }
    }

    pub fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
            Self::Other => 0,
        }
    }

    pub fn is_numeric(self) -> bool {
        self != Self::Other
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    /// Parses numpy (`<f4`, `float32`) and Zarr v3 (`float32`) type names.
    pub fn from_name(s: &str) -> Self {
        let t = s.trim_start_matches(['<', '>', '|', '=']);
        match t {
            "i1" | "int8" => Self::I8,
            "i2" | "int16" => Self::I16,
            "i4" | "int32" => Self::I32,
            "i8" | "int64" => Self::I64,
            "u1" | "uint8" => Self::U8,
            "u2" | "uint16" => Self::U16,
            "u4" | "uint32" => Self::U32,
            "u8" | "uint64" => Self::U64,
            "f4" | "float32" => Self::F32,
            "f8" | "float64" => Self::F64,
            _ => Self::Other,
        }
    }
}

/// Role of a dimension of a data variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DimRole {
    Time,
    Vertical,
    Horizontal,
    /// Any other dimension (ensemble member, bounds, ...). Listed, but operators reject it.
    Other,
}

/// One dimension of a variable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VarDim {
    pub name: String,
    pub size: usize,
    pub role: DimRole,
}

/// What a stored variable is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VarKind {
    /// A data variable (shown by `showname`, processed by operators).
    Data,
    /// A dimension coordinate (1-D, named like its dimension) or the time variable.
    Coordinate,
    /// An auxiliary coordinate named in a `coordinates` attribute.
    Auxiliary,
    /// A bounds variable (named in a `bounds` or `climatology` attribute).
    Bounds,
    /// A grid-mapping variable (named in a `grid_mapping` attribute).
    GridMapping,
    /// Cell measures and other variables not processed (strings, scalars without data use).
    Ancillary,
}

/// Missing-value and packing information of a variable.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Encoding {
    /// Values equal to one of these (compared in the stored type, before unpacking) are missing.
    pub missing: Vec<f64>,
    /// `scale_factor`, if present.
    pub scale_factor: Option<f64>,
    /// `add_offset`, if present.
    pub add_offset: Option<f64>,
    /// Whether unpacked values are 32-bit (scale/offset stored as float32 or stored type float32).
    pub unpacked_f32: bool,
    /// `_Unsigned = "true"` on signed integer storage: values are read as unsigned.
    pub unsigned: bool,
    /// Stored values (after the unsigned interpretation, before unpacking) outside
    /// `[valid_min, valid_max]` are missing (`valid_range`, `valid_min`, `valid_max`).
    pub valid_min: Option<f64>,
    pub valid_max: Option<f64>,
}

impl Encoding {
    pub fn is_packed(&self) -> bool {
        self.scale_factor.is_some() || self.add_offset.is_some()
    }

    fn has_range(&self) -> bool {
        self.valid_min.is_some() || self.valid_max.is_some()
    }

    /// Whether a stored value (as f64) lies outside the valid range.
    pub fn out_of_range(&self, v: f64) -> bool {
        self.has_range()
            && (self.valid_min.is_some_and(|m| v < m) || self.valid_max.is_some_and(|m| v > m))
    }
}

/// A stored variable (data or coordinate), metadata only.
#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub kind: VarKind,
    pub dtype: DType,
    pub dims: Vec<VarDim>,
    pub attrs: Attrs,
    /// Chunk shape per dimension (the whole extent for unchunked storage).
    pub chunks: Vec<usize>,
    pub encoding: Encoding,
    /// Index into [`Dataset::grids`] for data variables with a horizontal grid.
    pub grid: Option<usize>,
    /// Index into [`Dataset::zaxes`] for data variables with a vertical dimension.
    pub zaxis: Option<usize>,
}

impl Variable {
    pub fn shape(&self) -> Vec<usize> {
        self.dims.iter().map(|d| d.size).collect()
    }

    pub fn dim_names(&self) -> Vec<&str> {
        self.dims.iter().map(|d| d.name.as_str()).collect()
    }

    pub fn has_time(&self) -> bool {
        self.dims.iter().any(|d| d.role == DimRole::Time)
    }

    pub fn units(&self) -> Option<&str> {
        self.attrs.get_str("units")
    }

    pub fn long_name(&self) -> Option<&str> {
        self.attrs.get_str("long_name")
    }

    pub fn len(&self) -> usize {
        self.dims.iter().map(|d| d.size).product()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Storage format of an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Zarr2,
    Zarr3,
    NetCdf,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Self::Zarr2 => "zarr2",
            Self::Zarr3 => "zarr3",
            Self::NetCdf => "netcdf",
        }
    }
}

/// A dataset description: global attributes, all stored variables, and the derived grids,
/// vertical axes and time axis.
#[derive(Debug, Clone)]
pub struct Dataset {
    /// Path or URL the dataset was opened from.
    pub source: String,
    pub format: Format,
    pub attrs: Attrs,
    /// Dimensions in first-use order: (name, size).
    pub dims: Vec<(String, usize)>,
    /// All stored variables in file order (data variables have `kind == Data`).
    pub vars: Vec<Variable>,
    pub grids: Vec<Grid>,
    pub zaxes: Vec<ZAxis>,
    pub time: Option<TimeAxis>,
}

impl Dataset {
    /// Data variables in file order (what `showname` lists).
    pub fn data_vars(&self) -> impl Iterator<Item = &Variable> {
        self.vars.iter().filter(|v| v.kind == VarKind::Data)
    }

    pub fn var(&self, name: &str) -> Option<&Variable> {
        self.vars.iter().find(|v| v.name == name)
    }

    pub fn dim_size(&self, name: &str) -> Option<usize> {
        self.dims.iter().find(|(n, _)| n == name).map(|&(_, s)| s)
    }
}
