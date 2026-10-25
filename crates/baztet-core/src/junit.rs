//! Junit `test.xml` parsing: per-case results for the CLI and the BEP
//! test-summary resolution against the execution root.

use baztet_bep::BepOutput;

use crate::bazel::BazelRunner;

/// bazel junit test.xml files are XML 1.0; quick-xml 0.42 normalizes
/// attribute/text values per XML version.
pub const XML_VERSION: quick_xml::XmlVersion = quick_xml::XmlVersion::Implicit1_0;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TestCase {
    pub class_name: String,
    pub name: String,
    pub status: TestCaseStatus,
    pub duration_seconds: Option<f64>,
    /// failure/expectation message, if any
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum TestCaseStatus {
    Passed,
    Failed,
    Skipped,
    Error,
}

/// Parse a junit `test.xml` into cases.
pub fn parse_test_xml(bytes: &[u8]) -> Vec<TestCase> {
    let text = String::from_utf8_lossy(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut cases: Vec<TestCase> = Vec::new();
    let mut in_case = false;
    let mut current = TestCase {
        class_name: String::new(),
        name: String::new(),
        status: TestCaseStatus::Passed,
        duration_seconds: None,
        message: None,
    };
    let mut current_failure: Option<String> = None;
    use quick_xml::events::Event;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == "testcase" => {
                in_case = true;
                current = TestCase {
                    class_name: String::new(),
                    name: String::new(),
                    status: TestCaseStatus::Passed,
                    duration_seconds: None,
                    message: None,
                };
                current_failure = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        "classname" => {
                            current.class_name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "name" => {
                            current.name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "time" => {
                            current.duration_seconds = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .parse()
                                .ok()
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Start(e))
                if in_case && (e.name().as_ref() == "failure" || e.name().as_ref() == "error") =>
            {
                current.status = if e.name().as_ref() == "failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "message")
                {
                    current_failure = Some(
                        attr.normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
            }
            Ok(Event::Empty(e))
                if in_case && (e.name().as_ref() == "failure" || e.name().as_ref() == "error") =>
            {
                current.status = if e.name().as_ref() == "failure" {
                    TestCaseStatus::Failed
                } else {
                    TestCaseStatus::Error
                };
                if let Some(attr) = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == "message")
                {
                    current_failure = Some(
                        attr.normalized_value(XML_VERSION)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
            }
            Ok(Event::Empty(e)) if in_case && e.name().as_ref() == "skipped" => {
                current.status = TestCaseStatus::Skipped;
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == "testcase" => {
                // self-closing testcase: passed, attributes only
                let mut case = TestCase {
                    class_name: String::new(),
                    name: String::new(),
                    status: TestCaseStatus::Passed,
                    duration_seconds: None,
                    message: None,
                };
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        "classname" => {
                            case.class_name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "name" => {
                            case.name = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .into_owned()
                        }
                        "time" => {
                            case.duration_seconds = attr
                                .normalized_value(XML_VERSION)
                                .unwrap_or_default()
                                .parse()
                                .ok()
                        }
                        _ => {}
                    }
                }
                cases.push(case);
            }
            Ok(Event::End(e)) if e.name().as_ref() == "testcase" => {
                current.message = current_failure.take();
                cases.push(current.clone());
                in_case = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                eprintln!(
                    "[baztet] junit xml truncated/unparsable near {}: {e}",
                    reader.buffer_position()
                );
                break;
            }
            _ => {}
        }
    }
    cases
}

/// junit cases per test label, resolved against the execution root.
pub fn test_cases_from_bep(
    runner: &BazelRunner,
    bep: &BepOutput,
) -> Vec<(String /* label */, Vec<TestCase>)> {
    let execution_root = &runner.info.execution_root;
    let mut out: Vec<(String, Vec<TestCase>)> = Vec::new();
    for (label, cases) in group_by_label(bep) {
        let mut all: Vec<TestCase> = Vec::new();
        for path in cases {
            let full = execution_root.join(&path);
            if let Ok(bytes) = std::fs::read(full) {
                all.extend(parse_test_xml(&bytes));
            }
        }
        out.push((label, all));
    }
    out
}

fn group_by_label(bep: &BepOutput) -> Vec<(String, Vec<String>)> {
    // flaky tests re-run: only the LAST attempt's xml reflects the final
    // result; attempt 1 failures would otherwise be reported as active
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for summary in bep.test_results.iter().rev() {
        if out.iter().any(|(l, _)| *l == summary.label) {
            continue;
        }
        if let Some(xml) = &summary.xml {
            out.push((summary.label.clone(), vec![xml.relative_path()]));
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUNIT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.x.A" tests="3" failures="1" errors="0">
  <testcase name="okCase" classname="com.x.A" time="0.05"/>
  <testcase name="badCase" classname="com.x.A" time="0.10">
    <failure message="expected 42">stack...</failure>
  </testcase>
  <testcase name="skippedCase" classname="com.x.A">
    <skipped/>
  </testcase>
</testsuite>
"#;

    #[test]
    fn junit_parse() {
        let cases = parse_test_xml(JUNIT.as_bytes());
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].status, TestCaseStatus::Passed);
        assert_eq!(cases[0].duration_seconds, Some(0.05));
        assert_eq!(cases[1].status, TestCaseStatus::Failed);
        assert_eq!(cases[1].message.as_deref(), Some("expected 42"));
        assert_eq!(cases[2].status, TestCaseStatus::Skipped);
        assert_eq!(cases[0].class_name, "com.x.A");
    }

    #[test]
    fn junit_error_case() {
        let xml = r#"<testsuite><testcase name="e"><error message="boom"/></testcase></testsuite>"#;
        let cases = parse_test_xml(xml.as_bytes());
        assert_eq!(cases[0].status, TestCaseStatus::Error);
        assert_eq!(cases[0].message.as_deref(), Some("boom"));
    }

    #[test]
    fn last_attempt_wins_in_reports() {
        use baztet_bep::{BepFile, TestResultDetail};
        let bep = BepOutput {
            test_results: vec![
                // flaky rerun: attempt 1 failed, attempt 2 passed
                TestResultDetail {
                    label: "//a:t".into(),
                    status: "FAILED".into(),
                    cached_locally: false,
                    xml: Some(BepFile {
                        path_prefix: vec![],
                        name: "a/first/test.xml".into(),
                    }),
                    log: None,
                },
                TestResultDetail {
                    label: "//a:t".into(),
                    status: "PASSED".into(),
                    cached_locally: false,
                    xml: Some(BepFile {
                        path_prefix: vec![],
                        name: "a/second/test.xml".into(),
                    }),
                    log: None,
                },
            ],
            ..Default::default()
        };
        let groups = group_by_label(&bep);
        // only the LAST attempt's xml is reported
        assert_eq!(
            groups,
            [("//a:t".to_string(), vec!["a/second/test.xml".to_string()])]
        );
    }
}
