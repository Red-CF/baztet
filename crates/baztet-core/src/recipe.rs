//! IntelliJ Bazel run configurations (`.bazel/*.xml`) as CLI recipes.

use std::collections::BTreeMap;

use crate::junit::XML_VERSION;

/// The run config as a CLI recipe.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Recipe {
    pub name: String,
    /// bazel command: test | run | build
    pub command: String,
    pub target: String,
    pub env: BTreeMap<String, String>,
}

pub fn parse_recipe(xml: &str) -> Option<Recipe> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut recipe = Recipe::default();
    let mut in_blaze = false;
    let mut in_envs = false;
    loop {
        let ev = reader.read_event().ok()?;
        match ev {
            Event::Start(e) => match e.name().as_ref() {
                "configuration" => {
                    if let Some(attr) = e.attributes().flatten().find(|a| a.key.as_ref() == "name")
                    {
                        recipe.name = attr
                            .normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned();
                    }
                }
                "blaze-settings" => {
                    in_blaze = true;
                    if let Some(attr) = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == "blaze-command")
                    {
                        recipe.command = attr
                            .normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned();
                    }
                }
                "env_state" => in_envs = true,
                "env" if in_envs => {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
                _ => {}
            },
            Event::Empty(e) => {
                if in_envs && e.name().as_ref() == "env" {
                    let (name, value) = env_entry(&e);
                    recipe.env.insert(name, value);
                }
            }
            Event::Text(t) => {
                if in_blaze && !in_envs {
                    let text = t.xml_content(XML_VERSION).into_owned();
                    if text.contains("//") {
                        recipe.target = text;
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if recipe.target.is_empty() || recipe.command.is_empty() {
        return None;
    }
    Some(recipe)
}

fn env_entry(e: &quick_xml::events::BytesStart) -> (String, String) {
    let mut name = String::new();
    let mut value = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            "name" => {
                name = attr
                    .normalized_value(XML_VERSION)
                    .unwrap_or_default()
                    .into_owned()
            }
            "value" => {
                value = attr
                    .normalized_value(XML_VERSION)
                    .unwrap_or_default()
                    .into_owned()
            }
            _ => {}
        }
    }
    (name, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_parse_full() {
        let xml = r#"<configuration name="api unit tests" type="BlazeCommandRunConfigurationType" factoryName="Bazel Command">
  <blaze-settings handler-id="BlazeCommandGenericRunConfigurationHandlerProvider" kind="java_test" blaze-command="test">
    <blaze-target>//pkg/api:unit</blaze-target>
    <env_state>
      <envs>
        <env name="CONFIG_PATH" value="config.yaml"/>
      </envs>
    </env_state>
  </blaze-settings>
</configuration>"#;
        let r = parse_recipe(xml).expect("parses");
        assert_eq!(r.name, "api unit tests");
        assert_eq!(r.command, "test");
        assert_eq!(r.target, "//pkg/api:unit");
        assert_eq!(
            r.env.get("CONFIG_PATH").map(String::as_str),
            Some("config.yaml")
        );
    }

    #[test]
    fn recipe_rejects_incomplete() {
        assert!(parse_recipe(
            r#"<configuration><blaze-settings></blaze-settings></configuration>"#
        )
        .is_none());
        // target without command
        assert!(parse_recipe(
            r#"<configuration><blaze-settings blaze-command=""><blaze-target>//a:b</blaze-target></blaze-settings></configuration>"#
        )
        .is_none());
    }
}
