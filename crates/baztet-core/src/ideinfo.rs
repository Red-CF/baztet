//! Parsing aspect output files (`*.intellij-info.txt`, `TargetIdeInfo`
//! textprotos) into snapshot summaries.

use std::collections::BTreeMap;

use baztet_snapshot::{ConfigurationLite, IdeSummary, JarPair};
use prost_reflect::DynamicMessage;
use prost_reflect::Value;

/// Parse one `TargetIdeInfo` textproto file. Unknown fields are an error
/// (the Kotlin SDK allows them; callers skip unparsable targets and log).
pub fn parse(text: &str) -> Result<DynamicMessage, String> {
    DynamicMessage::parse_text_format(baztet_protos::target_ide_info_descriptor(), text)
        .map_err(|e| e.to_string())
}

/// Extract (label, configuration) and the trimmed summary from a parsed
/// TargetIdeInfo.
pub fn summarize(msg: &DynamicMessage) -> (Option<(String, String)>, IdeSummary) {
    let key = msg.get_field_by_name("key").and_then(|v| v.as_message().cloned());
    let identity = key.as_ref().map(|k| {
        let label = string_field(k, "label").unwrap_or_default();
        let configuration = string_field(k, "configuration").unwrap_or_default();
        (label, configuration)
    });

    let deps: Vec<String> = msg
        .get_field_by_name("deps")
        .and_then(|v| v.as_list().map(<[Value]>::to_vec))
        .unwrap_or_default()
        .iter()
        .filter_map(|d| {
            let target = d.as_message()?.get_field_by_name("target")?;
            string_field(target.as_ref().as_message()?, "label")
        })
        .collect();

    let srcs: Vec<String> = repeated_relative_paths(msg, "srcs");

    let mut java_jars: Vec<JarPair> = Vec::new();
    if let Some(java_info) = msg.get_field_by_name("java_common").and_then(|v| v.as_message().cloned()) {
        for jars in java_info
            .get_field_by_name("jars")
            .and_then(|v| v.as_list().map(<[Value]>::to_vec))
            .unwrap_or_default()
        {
            let Some(out) = jars.as_message() else { continue };
            let interface = repeated_relative_paths(out, "interface_jars").into_iter().next();
            let jar = repeated_relative_paths(out, "binary_jars").into_iter().next();
            if interface.is_some() || jar.is_some() {
                java_jars.push(JarPair { interface_jar: interface, jar });
            }
        }
    }

    let summary = IdeSummary {
        kind: String::new(),
        deps,
        srcs,
        java_jars,
        jvm_dir: None,
    };
    (identity, summary)
}

/// Workspace configurations from BEP events, trimmed for the snapshot.
pub fn configurations_from_bep(
    configs: &BTreeMap<String, baztet_bep::Configuration>,
) -> BTreeMap<String, ConfigurationLite> {
    configs
        .iter()
        .map(|(id, cfg)| {
            (
                id.clone(),
                ConfigurationLite { mnemonic: cfg.mnemonic.clone(), cpu: cfg.cpu.clone() },
            )
        })
        .collect()
}

/// Paths of a repeated `ArtifactLocation` field.
fn repeated_relative_paths(msg: &DynamicMessage, field: &str) -> Vec<String> {
    msg.get_field_by_name(field)
        .and_then(|v| v.as_list().map(<[Value]>::to_vec))
        .unwrap_or_default()
        .iter()
        .filter_map(|v| string_field(v.as_message()?, "relative_path"))
        .collect()
}

fn string_field(msg: &DynamicMessage, name: &str) -> Option<String> {
    msg.get_field_by_name(name).and_then(|v| v.as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
key {
  label: "//a:b"
  configuration: "cfg7"
}
deps {
  target { label: "//c:d" }
  dependency_type: COMPILE_TIME
}
srcs { relative_path: "a/Src.java" is_source: true }
java_common {
  jars {
    interface_jars { relative_path: "bazel-out/x/bin/a/libb-interface.jar" }
    binary_jars { relative_path: "bazel-out/x/bin/a/libb.jar" }
  }
}
"#;

    #[test]
    fn parse_and_summarize() {
        let msg = parse(SAMPLE).unwrap();
        let (identity, summary) = summarize(&msg);
        assert_eq!(identity, Some(("//a:b".into(), "cfg7".into())));
        assert_eq!(summary.deps, ["//c:d"]);
        assert_eq!(summary.srcs, ["a/Src.java"]);
        assert_eq!(summary.java_jars.len(), 1);
        assert_eq!(summary.java_jars[0].interface_jar.as_deref(), Some("bazel-out/x/bin/a/libb-interface.jar"));
        assert_eq!(summary.java_jars[0].jar.as_deref(), Some("bazel-out/x/bin/a/libb.jar"));
    }

    #[test]
    fn unknown_field_is_error() {
        assert!(parse("what_is_this: 1").is_err());
    }
}