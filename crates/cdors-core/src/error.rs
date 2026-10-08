//! Errors with stable string codes, hints and process exit codes.
//!
//! Every failure that reaches the user carries a [`ErrorCode`] (a stable snake_case string that
//! agents can match on), a human-readable message, an optional hint on how to fix it, and
//! optional extra fields (operator name, byte counts, ...). It renders either as plain text or as
//! one line of JSON:
//!
//! ```json
//! {"error":"unknown_operator","message":"...","operator":"yearmeans","hint":"did you mean yearmean?"}
//! ```

use serde_json::{Map, Value};

/// Stable error codes. The string form (see [`ErrorCode::as_str`]) is part of the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The operator name is not known.
    UnknownOperator,
    /// The operator is known (CDO name, registered) but not implemented yet.
    NotImplemented,
    /// Wrong number or type of arguments, unknown option, malformed chain.
    BadArguments,
    /// An input file or store does not exist or cannot be recognised.
    MissingInput,
    /// The output exists and `-O` was not given.
    OutputExists,
    /// A variable has no usable horizontal coordinates.
    NoCoordinates,
    /// The grid kind is not supported by the operator.
    UnsupportedGrid,
    /// A dimension (or its role) is not supported by the operator.
    UnsupportedDimension,
    /// The planned read exceeds `--max-read`.
    ReadLimit,
    /// An intermediate result between stages does not fit in memory.
    IntermediateTooLarge,
    /// An I/O failure that is worth retrying (network, transient storage errors).
    IoError,
    /// A malformed or unsupported file (corrupt metadata, unsupported data type or codec).
    BadData,
    /// A bug in cdors.
    Internal,
}

impl ErrorCode {
    /// The stable string form of the code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownOperator => "unknown_operator",
            Self::NotImplemented => "not_implemented",
            Self::BadArguments => "bad_arguments",
            Self::MissingInput => "missing_input",
            Self::OutputExists => "output_exists",
            Self::NoCoordinates => "no_coordinates",
            Self::UnsupportedGrid => "unsupported_grid",
            Self::UnsupportedDimension => "unsupported_dimension",
            Self::ReadLimit => "read_limit",
            Self::IntermediateTooLarge => "intermediate_too_large",
            Self::IoError => "io_error",
            Self::BadData => "bad_data",
            Self::Internal => "internal",
        }
    }

    /// Process exit code: 1 usage, 2 data, 3 retryable I/O, 4 refused (limit or existing output).
    pub fn exit_code(self) -> i32 {
        match self {
            Self::UnknownOperator
            | Self::NotImplemented
            | Self::BadArguments
            | Self::MissingInput => 1,
            Self::NoCoordinates
            | Self::UnsupportedGrid
            | Self::UnsupportedDimension
            | Self::IntermediateTooLarge
            | Self::BadData
            | Self::Internal => 2,
            Self::IoError => 3,
            Self::ReadLimit | Self::OutputExists => 4,
        }
    }

    /// Whether repeating the same command may succeed.
    pub fn retryable(self) -> bool {
        matches!(self, Self::IoError)
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A cdors error: code, message, optional hint and extra machine-readable fields.
#[derive(Debug, Clone)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    /// Extra fields for the JSON form, in insertion order (e.g. `operator`, `variable`, `bytes`).
    pub fields: Vec<(String, Value)>,
}

/// Result alias used throughout cdors.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            fields: Vec::new(),
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Adds an extra field to the JSON form (e.g. `("operator", "yearmeans")`).
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.push((key.to_owned(), value.into()));
        self
    }

    pub fn bad_arguments(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadArguments, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::IoError, message)
    }

    pub fn bad_data(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadData, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    pub fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }

    /// One-line JSON: `{"error": code, "message": ..., <fields>..., "retryable": bool, "hint": ...}`.
    pub fn to_json(&self) -> String {
        let mut m = Map::new();
        m.insert("error".into(), Value::from(self.code.as_str()));
        m.insert("message".into(), Value::from(self.message.clone()));
        for (k, v) in &self.fields {
            m.insert(k.clone(), v.clone());
        }
        m.insert("exit_code".into(), Value::from(self.exit_code()));
        m.insert("retryable".into(), Value::from(self.code.retryable()));
        if let Some(h) = &self.hint {
            m.insert("hint".into(), Value::from(h.clone()));
        }
        Value::Object(m).to_string()
    }

    /// Plain text: `cdors: error [code]: message` plus a `hint:` line.
    pub fn to_text(&self) -> String {
        let mut s = format!("cdors: error [{}]: {}", self.code, self.message);
        if let Some(h) = &self.hint {
            s.push_str("\n  hint: ");
            s.push_str(h);
        }
        s
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Error::new(ErrorCode::MissingInput, e.to_string()),
            _ => Error::io(e.to_string()),
        }
    }
}

impl From<netcdf::Error> for Error {
    fn from(e: netcdf::Error) -> Self {
        Error::io(format!("netCDF: {e}"))
    }
}
