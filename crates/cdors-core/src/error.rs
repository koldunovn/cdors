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
    /// The running state of one lane (one block of cells) does not fit in the memory budget.
    MemoryLimit,
    /// An I/O failure that is worth retrying (timeouts, connection resets and refusals,
    /// EAGAIN/EINTR, HTTP 5xx and 429).
    IoError,
    /// An I/O failure not known to be transient (e.g. EIO); repeating the command will
    /// probably fail the same way.
    IoFailed,
    /// Permission denied, or a read-only file system.
    PermissionDenied,
    /// No space left on the device, or the disk quota is exceeded.
    NoSpace,
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
            Self::MemoryLimit => "memory_limit",
            Self::IoError => "io_error",
            Self::IoFailed => "io_failed",
            Self::PermissionDenied => "permission_denied",
            Self::NoSpace => "no_space",
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
            | Self::MissingInput
            | Self::PermissionDenied
            | Self::NoSpace => 1,
            Self::NoCoordinates
            | Self::UnsupportedGrid
            | Self::UnsupportedDimension
            | Self::IntermediateTooLarge
            | Self::MemoryLimit
            | Self::BadData
            | Self::IoFailed
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

    /// An I/O failure known only by its message (errors of libraries that render the OS error
    /// as text): classified by [`classify_message`].
    pub fn io(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::new(classify_message(&message), message)
    }

    /// An I/O failure from an OS error, classified by its kind and errno ([`classify_io`]).
    pub fn from_io(e: &std::io::Error, message: impl Into<String>) -> Self {
        Self::new(classify_io(e), message)
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

/// Code of an OS error: only transient failures are the retryable `io_error`.
pub fn classify_io(e: &std::io::Error) -> ErrorCode {
    use std::io::ErrorKind as K;
    match e.raw_os_error() {
        Some(libc::EDQUOT | libc::ENOSPC) => return ErrorCode::NoSpace,
        Some(libc::EROFS | libc::EACCES | libc::EPERM) => return ErrorCode::PermissionDenied,
        Some(libc::ENOTDIR | libc::EISDIR | libc::ENAMETOOLONG | libc::ELOOP) => {
            return ErrorCode::BadArguments;
        }
        Some(libc::EEXIST) => return ErrorCode::OutputExists,
        Some(libc::ENOENT) => return ErrorCode::MissingInput,
        Some(
            libc::EAGAIN
            | libc::EINTR
            | libc::ETIMEDOUT
            | libc::ECONNRESET
            | libc::ECONNREFUSED
            | libc::ECONNABORTED
            | libc::EHOSTUNREACH
            | libc::ENETUNREACH
            | libc::ENETDOWN,
        ) => return ErrorCode::IoError,
        Some(_) => return ErrorCode::IoFailed,
        None => {}
    }
    match e.kind() {
        K::NotFound => ErrorCode::MissingInput,
        K::PermissionDenied | K::ReadOnlyFilesystem => ErrorCode::PermissionDenied,
        K::StorageFull | K::QuotaExceeded => ErrorCode::NoSpace,
        K::NotADirectory | K::IsADirectory | K::InvalidInput => ErrorCode::BadArguments,
        K::AlreadyExists => ErrorCode::OutputExists,
        K::InvalidData | K::UnexpectedEof => ErrorCode::BadData,
        K::TimedOut
        | K::ConnectionReset
        | K::ConnectionRefused
        | K::ConnectionAborted
        | K::WouldBlock
        | K::Interrupted
        | K::HostUnreachable
        | K::NetworkUnreachable
        | K::NetworkDown => ErrorCode::IoError,
        _ => ErrorCode::IoFailed,
    }
}

/// Code of an I/O failure known only by its message (libraries that render errors as text).
pub fn classify_message(msg: &str) -> ErrorCode {
    let m = msg.to_ascii_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains(k));
    if has(&[
        "quota exceeded",
        "no space left",
        "os error 122",
        "os error 28",
    ]) {
        ErrorCode::NoSpace
    } else if has(&[
        "permission denied",
        "read-only file system",
        "operation not permitted",
        "os error 13",
        "os error 30",
        "os error 1)",
    ]) {
        ErrorCode::PermissionDenied
    } else if has(&[
        "not a directory",
        "is a directory",
        "os error 20",
        "os error 21",
    ]) {
        ErrorCode::BadArguments
    } else if has(&["no such file", "os error 2)"]) {
        ErrorCode::MissingInput
    } else if has(&[
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "connection aborted",
        "connection closed",
        "temporarily unavailable",
        "try again",
        "interrupted system call",
        "error sending request",
        "dns error",
        "too many requests",
        "status: 429",
        "status: 5",
        "server error",
        "network is unreachable",
        "no route to host",
    ]) {
        ErrorCode::IoError
    } else {
        ErrorCode::IoFailed
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::from_io(&e, e.to_string())
    }
}

impl From<netcdf::Error> for Error {
    fn from(e: netcdf::Error) -> Self {
        let msg = format!("netCDF: {e}");
        match &e {
            // netCDF-C passes system errors through as positive errno values
            netcdf::Error::Netcdf(code) if *code > 0 => {
                Error::from_io(&std::io::Error::from_raw_os_error(*code), msg)
            }
            // NC_EPERM, NC_EEXIST, NC_ENOTNC, NC_EHDFERR (HDF5 rejected the file)
            netcdf::Error::Netcdf(-37) => Error::new(ErrorCode::PermissionDenied, msg),
            netcdf::Error::Netcdf(-35) => Error::new(ErrorCode::OutputExists, msg),
            netcdf::Error::Netcdf(-51 | -101) => Error::new(ErrorCode::BadData, msg),
            _ => Error::io(msg),
        }
    }
}
