use std::env;
use std::fmt;

pub type Result<T = (), E = Failed> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct Failed {
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl<E> From<E> for Failed
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(source: E) -> Self {
        if env::var("RUST_BACKTRACE").is_ok() {
            eprintln!("Failed: {source:#?}\n");
            eprintln!("Backtrace:");
            backtrace::trace(|frame| {
                backtrace::resolve_frame(frame, |symbol| {
                    if let (Some(name), Some(filename), Some(colno)) = (symbol.name(), symbol.filename(), symbol.colno())
                        && filename.components().any(|c| c.as_os_str().to_str() == Some("s3s"))
                    {
                        eprintln!("{name}\n  at {}:{colno}\n", filename.display());
                    }
                });
                true
            });
        }
        Self {
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = &self.source {
            write!(f, "Failed: {source}")
        } else {
            write!(f, "Failed")
        }
    }
}

impl Failed {
    pub fn from_string(s: impl Into<String>) -> Self {
        Self {
            source: Some(s.into().into()),
        }
    }
}
