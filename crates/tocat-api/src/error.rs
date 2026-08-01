//! Errors crossing the plugin boundary.
//!
//! Deliberately string-based rather than a rich enum: a WASM guest can only
//! hand back bytes, so anything richer would be lossy on one side of the
//! boundary and misleading on the other.

use std::fmt;

pub type Result<T, E = PluginError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// No plugin with this name is registered.
    Unknown {
        name: String,
        available: Vec<String>,
    },
    /// The plugin rejected its configuration.
    Config { plugin: String, message: String },
    /// The plugin failed while processing bytes.
    Runtime { plugin: String, message: String },
    /// The host could not satisfy a request made by a plugin.
    Host(String),
}

impl PluginError {
    /// Build an unknown plugin error
    /// 
    /// Carries the registry's contents, because "is that feature enabled?" and
    /// "did I put an endpoint in a plugin slot?" are the two things anyone
    /// actually wants to know here.
    pub fn unknown<S: Into<String>>(
        name: impl Into<String>,
        available: impl IntoIterator<Item = S>,
    ) -> Self {
        Self::Unknown {
            name: name.into(),
            available: available.into_iter().map(Into::into).collect(),
        }
    }

    /// Build an invalid config error
    pub fn config(plugin: impl Into<String>, message: impl fmt::Display) -> Self {
        Self::Config {
            plugin: plugin.into(),
            message: message.to_string(),
        }
    }

    /// Build a plugin runtime error
    pub fn runtime(plugin: impl Into<String>, message: impl fmt::Display) -> Self {
        Self::Runtime {
            plugin: plugin.into(),
            message: message.to_string(),
        }
    }

    /// Build a host request failure error
    pub fn host(message: impl fmt::Display) -> Self {
        Self::Host(message.to_string())
    }

    #[must_use]
    pub fn plugin(&self) -> Option<&str> {
        match self {
            Self::Unknown { name, .. } => Some(name),
            Self::Config { plugin, .. } | Self::Runtime { plugin, .. } => Some(plugin),
            Self::Host(_) => None,
        }
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { name, available } => {
                write!(f, "unknown plugin `{name}`")?;

                if !available.is_empty() {
                    write!(f, " (compiled in: {})", available.join(", "))?;
                }

                Ok(())
            }
            Self::Config { plugin, message } => {
                write!(f, "invalid configuration for plugin `{plugin}`: {message}")
            }
            Self::Runtime { plugin, message } => write!(f, "plugin `{plugin}` failed: {message}"),
            Self::Host(message) => write!(f, "host error: {message}"),
        }
    }
}

impl std::error::Error for PluginError {}
