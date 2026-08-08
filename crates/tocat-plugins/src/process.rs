//! `process` - hand this path's bytes to a subprocess and take its stdout back.
//!
//! Any filter that reads stdin and writes stdout becomes a stage:
//!
//! ```toml
//! [[plugin]]
//! name = "process"
//! direction = "source-to-sink"
//! argv = ["gzip", "-c"]
//! ```
//!
//! This plugin never sees a byte. It validates configuration and describes the
//! child; the host spawns it and does the plumbing, because a subprocess cannot
//! satisfy the synchronous `Plugin` contract: it decides nothing per chunk and
//! may emit output belonging to chunks it was given long ago.
//!
//! # Cost
//!
//! Two copies and two syscalls per chunk in each direction, plus a process per
//! connection per direction. Under `fork` with `direction = "both"`, sixty-four
//! clients means one hundred and twenty-eight children. That is the correct
//! price for "any tool becomes a stage", but it should be paid knowingly:
//! prefer a native or WASM stage for anything hot.

use serde::{Deserialize, Serialize};
use tocat_api::{
    BuildCtx, Execution, ExternalStage, PluginError, PluginFactory, Result, Stage, StderrMode,
};

pub const NAME: &str = "process";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ProcessConfig {
    /// Program and arguments, passed directly: no shell, no globbing, no
    /// metacharacters.
    #[serde(default)]
    pub argv: Option<Vec<String>>,

    /// A shell command line. Runs with tocat's privileges. Do not build one
    /// from untrusted input, or accept one from a config file others can write.
    #[serde(default, alias = "cmd")]
    pub command: Option<String>,

    #[serde(default)]
    pub stderr: StderrMode,
}

pub struct ProcessFactory;

impl PluginFactory for ProcessFactory {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "pipe this direction through a subprocess"
    }

    fn execution(&self) -> Execution {
        // A child always has its own task. The host rejects `detach = false`.
        Execution::Detached
    }

    fn build(&self, ctx: &mut BuildCtx<'_>) -> Result<Stage> {
        let config: ProcessConfig = ctx.config()?;

        let (argv, shell) = match (config.argv, config.command) {
            (Some(_), Some(_)) => {
                return Err(PluginError::config(
                    NAME,
                    "`argv` and `command` are alternatives; give one",
                ));
            }
            (None, None) => {
                return Err(PluginError::config(NAME, "needs `argv` or `command`"));
            }
            (Some(argv), None) => {
                if argv.iter().any(String::is_empty) || argv.is_empty() {
                    return Err(PluginError::config(NAME, "`argv` has an empty entry"));
                }
                (argv, false)
            }
            (None, Some(command)) => {
                if command.trim().is_empty() {
                    return Err(PluginError::config(NAME, "`command` is empty"));
                }
                (vec![command], true)
            }
        };

        Ok(Stage::External(ExternalStage {
            argv,
            shell,
            stderr: config.stderr,
            name: ctx.stage().name.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tocat_api::{ChannelId, ChannelTarget, Direction, HostBuilder, PipelineMeta, StageInfo};

    use super::*;

    struct NullHost;

    impl HostBuilder for NullHost {
        fn open_channel(&mut self, _target: ChannelTarget) -> Result<ChannelId> {
            Ok(ChannelId(0))
        }
    }

    fn build(config: Value) -> Result<Stage> {
        let map = config.as_object().expect("object").clone();
        let meta = PipelineMeta::new(Direction::SourceToSink, "src", "sink");
        let mut host = NullHost;
        let stage = StageInfo {
            index: 0,
            total: 1,
            name: "gzip",
            upstream: "src",
            downstream: "sink",
        };
        let mut ctx = BuildCtx::new(NAME, &map, &meta, stage, &mut host);
        ProcessFactory.build(&mut ctx)
    }

    #[test]
    fn argv_form_takes_the_display_name() {
        match build(json!({ "argv": ["gzip", "-c"] })).unwrap() {
            Stage::External(e) => {
                assert_eq!(e.argv, ["gzip", "-c"]);
                assert!(!e.shell);
                assert_eq!(e.name, "gzip");
            }
            Stage::Filter(_) => panic!("process is external"),
        }
    }

    #[test]
    fn command_form_is_shelled() {
        match build(json!({ "command": "grep -v DEBUG | sort -u" })).unwrap() {
            Stage::External(e) => assert!(e.shell && e.argv.len() == 1),
            Stage::Filter(_) => panic!("process is external"),
        }
    }

    #[test]
    fn rejects_ambiguous_or_empty_invocations() {
        assert!(build(json!({})).is_err());
        assert!(build(json!({ "argv": [], "command": "cat" })).is_err());
        assert!(build(json!({ "argv": [] })).is_err());
        assert!(build(json!({ "command": "   " })).is_err());
    }
}
