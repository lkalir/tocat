use anyhow::bail;
use serde_json::{Map, Value};
use tocat_api::{DirectionSpec, PluginSpec, normalize};

/// Parse `NAME[:DIRECTION][,key=value]...`, e.g.
/// `tee:both,as=wire,file=session.hex,format=hex`.
///
/// `as` and `detach` are consumed by the host; everything else is handed to
/// the plugin.
///
/// Bare keys are `true`, so `tee,append` reads the way a flag should. Values
/// are coerced to bool/integer where they parse as one, since the plugin's
/// config type decides the real shape.
pub fn parse_plugin_spec(raw: &str) -> anyhow::Result<PluginSpec> {
    let mut parts = raw.split(',');
    let head = parts.next().unwrap_or_default().trim();

    if head.is_empty() {
        bail!("expected a plugin name");
    }

    let (name, direction) = match head.split_once(':') {
        Some((name, dir)) => (
            name,
            dir.parse::<DirectionSpec>()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        ),
        None => (head, DirectionSpec::default()),
    };

    if name.is_empty() {
        bail!("expected a plugin name");
    }

    let mut config = Map::new();
    let mut detach = None;
    let mut alias = None;

    for opt in parts {
        let opt = opt.trim();
        if opt.is_empty() {
            continue;
        }

        let (key, value) = match opt.split_once('=') {
            Some((key, value)) => (key, coerce(value)),
            None => (opt, Value::Bool(true)),
        };

        // Reserved in every spelling, since that is how they are matched.
        match normalize(key).as_str() {
            "detach" => {
                detach = value.as_bool();
            }
            "as" => {
                alias = value.as_str().map(str::to_string);
            }
            // The key goes on as the user wrote it. Matching it against what the plugin declares is
            // the plugin's own deserialization, which needs the original to recognize a
            // `#[serde(alias)]`.
            _ => {
                config.insert(key.to_string(), value);
            }
        }
    }

    Ok(PluginSpec {
        name: name.to_string(),
        direction,
        alias,
        detach,
        config,
    })
}

fn coerce(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => value
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(value.to_string())),
    }
}
