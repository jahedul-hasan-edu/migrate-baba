use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A database error with all the fields a user needs to fix the problem.
#[derive(Debug, Clone)]
pub struct DbError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub statement: Option<String>,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if !self.code.is_empty() {
            write!(f, " [SQLSTATE {}]", self.code)?;
        }
        if let Some(d) = &self.detail {
            write!(f, "\nDetail: {d}")?;
        }
        if let Some(h) = &self.hint {
            write!(f, "\nHint: {h}")?;
        }
        if let Some(s) = &self.statement {
            let s = if s.len() > 600 { format!("{}…", &s[..s.floor_char_boundary(600)]) } else { s.clone() };
            write!(f, "\nStatement: {s}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("{0}")]
    Db(DbError),
    #[error("operation cancelled")]
    Cancelled,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Convert a driver error, attaching the statement that caused it.
    pub fn from_pg(e: tokio_postgres::Error, statement: Option<&str>) -> Self {
        if let Some(db) = e.as_db_error() {
            Error::Db(DbError {
                code: db.code().code().to_string(),
                message: db.message().to_string(),
                detail: db.detail().map(str::to_string),
                hint: db.hint().map(str::to_string),
                statement: statement.map(str::to_string),
            })
        } else {
            let mut msg = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(": ");
                msg.push_str(&s.to_string());
                src = s.source();
            }
            Error::Db(DbError {
                code: String::new(),
                message: msg,
                detail: None,
                hint: None,
                statement: statement.map(str::to_string),
            })
        }
    }

    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Error::Db(d) if !d.code.is_empty() => Some(&d.code),
            _ => None,
        }
    }
}

/// Extension to attach statements to driver errors ergonomically.
pub trait PgResultExt<T> {
    fn sql(self, statement: &str) -> Result<T>;
}

impl<T> PgResultExt<T> for std::result::Result<T, tokio_postgres::Error> {
    fn sql(self, statement: &str) -> Result<T> {
        self.map_err(|e| Error::from_pg(e, Some(statement)))
    }
}
