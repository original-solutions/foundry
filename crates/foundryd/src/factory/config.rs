//! The factory block of a project's `.dw/config.yml`.

use std::fmt;

use serde_yaml_ng::{Mapping, Value};

/// Where the coordinator reads a project's config, on the default branch.
pub const PATH: &str = ".dw/config.yml";

const DEFAULT_MINUTES: u64 = 180;
const DEFAULT_CHECKS_MINUTES: u64 = 30;
const DEFAULT_INTERRUPTION_RESUMES: u64 = 2;

/// One problem found in the config: the file, the field it is about (none for a whole-file
/// problem) and what is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: String,
    pub field: Option<String>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    /// `<file>: <field>: <message>`, or `<file>: <message>` with no field.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.field {
            Some(field) => write!(f, "{}: {}: {}", self.file, field, self.message),
            None => write!(f, "{}: {}", self.file, self.message),
        }
    }
}

/// Every problem found in one file, collected in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors {
    pub file: String,
    pub errors: Vec<ConfigError>,
}

impl fmt::Display for ConfigErrors {
    /// Each error on its own line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// The time and retry limits of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub minutes: u64,
    pub gate_retries: u64,
    pub checks_minutes: u64,
    pub interruption_resumes: u64,
}

/// The `factory` block of a project's `.dw/config.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub instructors: Vec<String>,
    pub base: String,
    pub provision: Vec<String>,
    pub gate: Vec<String>,
    pub required_checks: Vec<String>,
    pub limits: Limits,
    /// Whether `factory.owner` is set. The value is not kept.
    pub owner_set: bool,
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn get<'a>(parent: &'a Mapping, key: &str) -> Option<&'a Value> {
    parent.get(Value::String(key.to_string()))
}

/// Collects every problem of one file. `None` stands for "no value": an absent key or a parent
/// that was already reported.
#[derive(Default)]
struct Checker {
    errors: Vec<ConfigError>,
}

impl Checker {
    fn err(&mut self, field: &str, message: &str) {
        self.errors.push(ConfigError {
            file: PATH.to_string(),
            field: (!field.is_empty()).then(|| field.to_string()),
            message: message.to_string(),
        });
    }

    fn child<'a>(
        &mut self,
        parent: Option<&'a Mapping>,
        path: &str,
        key: &str,
    ) -> Option<&'a Value> {
        let parent = parent?;
        let found = get(parent, key);
        if found.is_none() {
            self.err(&join(path, key), "is missing");
        }
        found
    }

    fn mapping<'a>(&mut self, value: Option<&'a Value>, path: &str) -> Option<&'a Mapping> {
        let value = value?;
        match value.as_mapping() {
            Some(m) => Some(m),
            None => {
                self.err(path, "must be a mapping");
                None
            }
        }
    }

    fn non_empty_string(&mut self, value: Option<&Value>, path: &str) -> Option<String> {
        let value = value?;
        match value.as_str() {
            None => {
                self.err(path, "must be a string");
                None
            }
            Some("") => {
                self.err(path, "must not be empty");
                None
            }
            Some(s) => Some(s.to_string()),
        }
    }

    /// An integer at or above `min`. A float, even a whole one, is not an integer.
    fn uint(&mut self, value: Option<&Value>, path: &str, min: u64) -> Option<u64> {
        let value = value?;
        match value {
            Value::Number(n) => match n.as_u64() {
                Some(v) if v >= min => Some(v),
                _ => {
                    self.err(path, &format!("must be an integer of at least {min}"));
                    None
                }
            },
            _ => {
                self.err(path, "must be an integer");
                None
            }
        }
    }

    /// Like `uint` for `key` under `parent`, but an absent key takes `fallback`.
    fn optional_uint(
        &mut self,
        parent: Option<&Mapping>,
        path: &str,
        key: &str,
        fallback: u64,
        min: u64,
    ) -> Option<u64> {
        let parent = parent?;
        match get(parent, key) {
            None => Some(fallback),
            found => self.uint(found, &join(path, key), min),
        }
    }

    fn string_list(
        &mut self,
        value: Option<&Value>,
        path: &str,
        allow_empty: bool,
    ) -> Option<Vec<String>> {
        let value = value?;
        let Some(items) = value.as_sequence() else {
            self.err(path, "must be a list of strings");
            return None;
        };
        if items.is_empty() && !allow_empty {
            self.err(path, "must not be empty");
            return None;
        }
        let before = self.errors.len();
        let out: Vec<String> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| self.non_empty_string(Some(item), &format!("{path}[{i}]")))
            .collect();
        (self.errors.len() == before).then_some(out)
    }

    fn autonomy(&mut self, value: Option<&Value>, path: &str) -> Option<()> {
        let value = value?;
        match value.as_str() {
            Some("pr") => Some(()),
            Some(other) => {
                self.err(
                    path,
                    &format!("unsupported value `{other}`; only `pr` is accepted in phase 1"),
                );
                None
            }
            None => {
                self.err(path, "must be a string; only `pr` is accepted in phase 1");
                None
            }
        }
    }

    fn limits(&mut self, limits: Option<&Mapping>, path: &str) -> Option<Limits> {
        let minutes = self.optional_uint(limits, path, "minutes", DEFAULT_MINUTES, 1);
        let gate_retries = {
            let found = self.child(limits, path, "gate_retries");
            self.uint(found, &join(path, "gate_retries"), 0)
        };
        let checks_minutes =
            self.optional_uint(limits, path, "checks_minutes", DEFAULT_CHECKS_MINUTES, 1);
        let interruption_resumes = self.optional_uint(
            limits,
            path,
            "interruption_resumes",
            DEFAULT_INTERRUPTION_RESUMES,
            0,
        );
        Some(Limits {
            minutes: minutes?,
            gate_retries: gate_retries?,
            checks_minutes: checks_minutes?,
            interruption_resumes: interruption_resumes?,
        })
    }

    fn project(&mut self, root: &Mapping) -> Option<ProjectConfig> {
        let found = self.child(Some(root), "", "factory");
        let factory = self.mapping(found, "factory");

        let found = self.child(factory, "factory", "instructors");
        let instructors = self.string_list(found, "factory.instructors", false);
        let found = self.child(factory, "factory", "base");
        let base = self.non_empty_string(found, "factory.base");
        let found = self.child(factory, "factory", "provision");
        let provision = self.string_list(found, "factory.provision", true);
        let found = self.child(factory, "factory", "gate");
        let gate = self.string_list(found, "factory.gate", false);

        let required_checks = match factory {
            None => None,
            Some(f) => match get(f, "required_checks") {
                None => Some(Vec::new()),
                found => self.string_list(found, "factory.required_checks", true),
            },
        };

        let found = self.child(factory, "factory", "limits");
        let limits_map = self.mapping(found, "factory.limits");
        let limits = self.limits(limits_map, "factory.limits");

        let found = self.child(factory, "factory", "autonomy");
        let autonomy = self.autonomy(found, "factory.autonomy");

        let owner_set = match factory {
            None => None,
            Some(f) => match get(f, "owner") {
                None => Some(false),
                found => self.non_empty_string(found, "factory.owner").map(|_| true),
            },
        };

        autonomy?;
        Some(ProjectConfig {
            instructors: instructors?,
            base: base?,
            provision: provision?,
            gate: gate?,
            required_checks: required_checks?,
            limits: limits?,
            owner_set: owner_set?,
        })
    }
}

fn fail(errors: Vec<ConfigError>) -> ConfigErrors {
    ConfigErrors {
        file: PATH.to_string(),
        errors,
    }
}

/// Parse the text of a project's `.dw/config.yml`, reporting every problem found in one pass.
/// Keys the factory does not read are ignored; `factory.owner` only sets `owner_set`.
pub fn parse(yaml: &str) -> Result<ProjectConfig, ConfigErrors> {
    let mut c = Checker::default();
    let value = match serde_yaml_ng::from_str::<Value>(yaml) {
        Ok(value) => value,
        Err(cause) => {
            c.err("", &format!("not valid YAML: {cause}"));
            return Err(fail(c.errors));
        }
    };
    let Some(root) = value.as_mapping() else {
        c.err("", "top level must be a mapping");
        return Err(fail(c.errors));
    };
    match c.project(root) {
        Some(config) if c.errors.is_empty() => Ok(config),
        _ => Err(fail(c.errors)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "
tracker:
  backend: do-work-io
factory:
  instructors: [tomkaczocha]
  base: main
  provision: [cargo fetch]
  gate:
    - cargo fmt --check
    - cargo test
  limits: { minutes: 90, gate_retries: 2 }
  autonomy: pr
";

    fn is_index(part: &str) -> bool {
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
    }

    fn step<'a>(cur: &'a mut Value, part: &str) -> &'a mut Value {
        let found = if is_index(part) {
            cur.get_mut(part.parse::<usize>().unwrap())
        } else {
            cur.get_mut(part)
        };
        found.unwrap_or_else(|| panic!("no {part} in the tree"))
    }

    fn parent_of<'a>(tree: &'a mut Value, parts: &[&str]) -> &'a mut Value {
        let mut cur = tree;
        for part in &parts[..parts.len() - 1] {
            cur = step(cur, part);
        }
        cur
    }

    /// Parse `base`, apply `f` to the tree, and return the edited text.
    fn edit(base: &str, f: impl FnOnce(&mut Value)) -> String {
        let mut tree: Value = serde_yaml_ng::from_str(base).unwrap();
        f(&mut tree);
        serde_yaml_ng::to_string(&tree).unwrap()
    }

    /// Set a dotted path (numeric segments index lists) to the value of the YAML text `yaml`.
    fn set(tree: &mut Value, path: &str, yaml: &str) {
        let parts: Vec<&str> = path.split('.').collect();
        let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
        let last = parts[parts.len() - 1];
        let parent = parent_of(tree, &parts);
        if is_index(last) {
            *parent.get_mut(last.parse::<usize>().unwrap()).unwrap() = value;
        } else {
            parent
                .as_mapping_mut()
                .unwrap()
                .insert(Value::String(last.to_string()), value);
        }
    }

    fn remove(tree: &mut Value, path: &str) {
        let parts: Vec<&str> = path.split('.').collect();
        let last = parts[parts.len() - 1];
        parent_of(tree, &parts)
            .as_mapping_mut()
            .unwrap()
            .remove(last);
    }

    fn project_err(text: &str) -> Vec<ConfigError> {
        match parse(text) {
            Ok(_) => panic!("expected invalid project config"),
            Err(e) => e.errors,
        }
    }

    fn project(text: &str) -> ProjectConfig {
        parse(text).unwrap_or_else(|e| panic!("unexpected errors: {e:?}"))
    }

    /// Assert exactly one error whose field is exactly as given, on the config file.
    fn assert_one(errs: &[ConfigError], field: &str) {
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].field.as_deref(), Some(field));
        assert_eq!(errs[0].file, PATH);
    }

    /// Assert exactly one file-level error (no field).
    fn assert_file_level(errs: &[ConfigError], message_part: &str) {
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].field, None);
        assert_eq!(errs[0].file, PATH);
        assert!(errs[0].message.contains(message_part), "{errs:?}");
    }

    fn fields(errs: &[ConfigError]) -> Vec<&str> {
        errs.iter().filter_map(|e| e.field.as_deref()).collect()
    }

    fn project_missing(path: &str) {
        assert_one(&project_err(&edit(PROJECT, |t| remove(t, path))), path);
    }

    fn project_bad(path: &str, yaml: &str) {
        assert_one(&project_err(&edit(PROJECT, |t| set(t, path, yaml))), path);
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    mod project_config {
        use super::*;

        mod valid {
            use super::*;

            #[test]
            fn project_valid_resolves_all_fields() {
                let c = project(PROJECT);
                assert_eq!(c.instructors, strs(&["tomkaczocha"]));
                assert_eq!(c.base, "main");
                assert_eq!(c.provision, strs(&["cargo fetch"]));
                assert_eq!(c.gate, strs(&["cargo fmt --check", "cargo test"]));
                assert_eq!(c.limits.minutes, 90);
                assert_eq!(c.limits.gate_retries, 2);
                assert!(c.required_checks.is_empty());
                assert_eq!(c.limits.checks_minutes, 30);
                assert!(!c.owner_set);
            }
        }

        mod the_retired_regates_key {
            use super::*;

            #[test]
            fn is_accepted_and_ignored_set_as_a_block_key_or_inline() {
                let c = project(&edit(PROJECT, |t| set(t, "factory.limits.regates", "12")));
                assert_eq!(
                    c.limits,
                    Limits {
                        minutes: 90,
                        gate_retries: 2,
                        checks_minutes: 30,
                        interruption_resumes: 2,
                    }
                );
                let inline = PROJECT.replace(
                    "limits: { minutes: 90, gate_retries: 2 }",
                    "limits: { minutes: 90, gate_retries: 2, regates: 3 }",
                );
                assert_eq!(project(&inline).limits, c.limits);
            }

            #[test]
            fn is_ignored_whatever_its_value() {
                let without = project(PROJECT).limits;
                for value in ["5", "0", "-1", "\"many\"", "[1]", "null"] {
                    let c = project(&edit(PROJECT, |t| set(t, "factory.limits.regates", value)));
                    assert_eq!(c.limits, without, "{value}");
                }
            }
        }

        mod the_required_checks {
            use super::*;

            const CHECKS: &str = "factory.required_checks";
            const CHECKS_MINUTES: &str = "factory.limits.checks_minutes";

            #[test]
            fn project_required_checks_are_kept_as_given() {
                let c = project(&edit(PROJECT, |t| {
                    set(t, CHECKS, "[gate, \"build (linux)\"]")
                }));
                assert_eq!(c.required_checks, strs(&["gate", "build (linux)"]));
            }

            #[test]
            fn project_required_checks_may_be_an_empty_list() {
                let c = project(&edit(PROJECT, |t| set(t, CHECKS, "[]")));
                assert!(c.required_checks.is_empty());
            }

            #[test]
            fn project_required_checks_must_be_a_list_of_non_empty_strings() {
                for bad in ["gate", "5", "null", "[1]", "[\"\"]", "[gate, \"\"]"] {
                    let errs = project_err(&edit(PROJECT, |t| set(t, CHECKS, bad)));
                    assert!(!errs.is_empty(), "{bad}");
                    for e in &errs {
                        assert!(
                            e.field.as_deref().is_some_and(|f| f.starts_with(CHECKS)),
                            "{bad}: {:?}",
                            e.field
                        );
                    }
                }
                let errs = project_err(&edit(PROJECT, |t| set(t, CHECKS, "[gate, \"\"]")));
                assert_one(&errs, "factory.required_checks[1]");
            }

            #[test]
            fn project_checks_minutes_defaults_to_30_and_can_be_set() {
                let at = |v: &str| {
                    project(&edit(PROJECT, |t| set(t, CHECKS_MINUTES, v)))
                        .limits
                        .checks_minutes
                };
                assert_eq!(at("45"), 45);
                assert_eq!(at("1"), 1);
            }

            #[test]
            fn project_checks_minutes_must_be_an_integer_of_at_least_one() {
                for bad in ["\"soon\"", "0", "-1", "1.5", "[1]", "null", "true"] {
                    let errs = project_err(&edit(PROJECT, |t| set(t, CHECKS_MINUTES, bad)));
                    assert_one(&errs, CHECKS_MINUTES);
                }
            }
        }

        mod the_owner {
            use super::*;

            const OWNER: &str = "factory.owner";

            #[test]
            fn project_owner_is_kept_as_given() {
                let c = project(&edit(PROJECT, |t| set(t, OWNER, "T3fac")));
                assert!(c.owner_set);
                assert_eq!(
                    c,
                    ProjectConfig {
                        owner_set: true,
                        ..project(PROJECT)
                    }
                );
            }

            #[test]
            fn project_owner_parses_sets_owner_set_and_changes_nothing_else() {
                let c = project(&edit(PROJECT, |t| set(t, OWNER, "t3fac")));
                let base = project(PROJECT);
                assert!(c.owner_set);
                assert!(!base.owner_set);
                assert_eq!(c.instructors, base.instructors);
                assert_eq!(c.base, base.base);
                assert_eq!(c.provision, base.provision);
                assert_eq!(c.gate, base.gate);
                assert_eq!(c.required_checks, base.required_checks);
                assert_eq!(c.limits, base.limits);
            }

            #[test]
            fn project_owner_is_null_when_the_key_is_absent() {
                assert!(!project(PROJECT).owner_set);
            }

            #[test]
            fn project_owner_must_be_a_non_empty_string() {
                for bad in ["\"\"", "5", "null", "[t3fac]", "{ name: t3fac }", "true"] {
                    assert_one(&project_err(&edit(PROJECT, |t| set(t, OWNER, bad))), OWNER);
                }
            }
        }

        #[test]
        fn project_ignores_the_needs_info_limits_an_older_version_read() {
            let t = PROJECT.replace(
                "limits: { minutes: 90, gate_retries: 2 }",
                "limits: { minutes: 90, gate_retries: 2, needs_info_nudge_hours: 5, needs_info_timeout_hours: 3, needs_info_rounds: 0 }",
            );
            assert_eq!(project(&t), project(PROJECT));
        }

        mod the_interruption_resumes {
            use super::*;

            const RESUMES: &str = "factory.limits.interruption_resumes";

            fn with_resumes(resumes: &str) -> String {
                edit(PROJECT, |t| set(t, RESUMES, resumes))
            }

            #[test]
            fn project_interruption_resumes_default_to_2() {
                assert_eq!(project(PROJECT).limits.interruption_resumes, 2);
            }

            #[test]
            fn project_interruption_resumes_can_be_set_and_can_be_zero() {
                for (text, want) in [("0", 0), ("1", 1), ("2", 2), ("10", 10)] {
                    assert_eq!(
                        project(&with_resumes(text)).limits.interruption_resumes,
                        want
                    );
                }
            }

            #[test]
            fn project_interruption_resumes_are_read_from_a_limits_block_written_inline() {
                let t = PROJECT.replace(
                    "limits: { minutes: 90, gate_retries: 2 }",
                    "limits: { minutes: 90, gate_retries: 2, interruption_resumes: 4 }",
                );
                assert_eq!(project(&t).limits.interruption_resumes, 4);
            }

            #[test]
            fn project_interruption_resumes_must_be_an_integer_of_at_least_zero() {
                for bad in ["\"many\"", "-1", "1.5", "[1]", "null", "true"] {
                    assert_one(&project_err(&with_resumes(bad)), RESUMES);
                }
                assert_eq!(
                    project_err(&with_resumes("-1"))[0].message,
                    "must be an integer of at least 0"
                );
            }
        }

        #[test]
        fn project_ignores_other_top_level_keys() {
            let t = "tracker: 5\nmodels: [a]\nfactory:\n  instructors: [a]\n  base: main\n  provision: []\n  gate: [make]\n  limits: { minutes: 1, gate_retries: 0 }\n  autonomy: pr\n";
            let c = project(t);
            assert!(c.provision.is_empty());
            assert_eq!(c.limits.gate_retries, 0);
        }

        #[test]
        fn project_ignores_unknown_keys_inside_the_factory_block() {
            let t = edit(PROJECT, |tree| set(tree, "factory.something_else", "5"));
            assert_eq!(project(&t), project(PROJECT));
        }

        mod missing_fields {
            use super::*;

            #[test]
            fn project_missing_factory_block() {
                let errs = project_err("tracker: x\n");
                assert_one(&errs, "factory");
                assert_eq!(errs[0].message, "is missing");
                assert_eq!(errs[0].to_string(), ".dw/config.yml: factory: is missing");
            }
            #[test]
            fn project_missing_instructors() {
                project_missing("factory.instructors");
            }
            #[test]
            fn project_missing_base() {
                project_missing("factory.base");
            }
            #[test]
            fn project_missing_provision() {
                project_missing("factory.provision");
            }
            #[test]
            fn project_missing_gate() {
                project_missing("factory.gate");
            }
            #[test]
            fn project_missing_limits() {
                project_missing("factory.limits");
            }
            #[test]
            fn project_limits_minutes_defaults_to_180_when_missing() {
                let t = edit(PROJECT, |tree| remove(tree, "factory.limits.minutes"));
                assert_eq!(project(&t).limits.minutes, 180);
            }
            #[test]
            fn project_missing_limits_gate_retries() {
                project_missing("factory.limits.gate_retries");
            }
            #[test]
            fn project_missing_autonomy() {
                project_missing("factory.autonomy");
            }
        }

        mod wrong_types_and_values {
            use super::*;

            #[test]
            fn project_factory_not_a_mapping() {
                assert_one(&project_err("factory: 3\n"), "factory");
            }
            #[test]
            fn project_instructors_not_a_list() {
                project_bad("factory.instructors", "tomkaczocha");
            }
            #[test]
            fn project_instructors_empty() {
                project_bad("factory.instructors", "[]");
            }
            #[test]
            fn project_instructors_item_not_a_string() {
                let t = edit(PROJECT, |tree| set(tree, "factory.instructors", "[a, 5]"));
                assert_one(&project_err(&t), "factory.instructors[1]");
            }
            #[test]
            fn project_instructors_empty_item() {
                let t = edit(PROJECT, |tree| set(tree, "factory.instructors", "[\"\"]"));
                assert_one(&project_err(&t), "factory.instructors[0]");
            }
            #[test]
            fn project_gate_empty_item() {
                let t = edit(PROJECT, |tree| set(tree, "factory.gate", "[\"\"]"));
                assert_one(&project_err(&t), "factory.gate[0]");
            }
            #[test]
            fn project_base_not_a_string() {
                project_bad("factory.base", "[main]");
            }
            #[test]
            fn project_base_empty() {
                project_bad("factory.base", "\"\"");
            }
            #[test]
            fn project_provision_not_a_list() {
                project_bad("factory.provision", "cargo fetch");
            }
            #[test]
            fn project_provision_item_not_a_string() {
                let t = edit(PROJECT, |tree| set(tree, "factory.provision", "[{a: b}]"));
                assert_one(&project_err(&t), "factory.provision[0]");
            }
            #[test]
            fn project_gate_not_a_list() {
                project_bad("factory.gate", "cargo test");
            }
            #[test]
            fn project_gate_empty() {
                project_bad("factory.gate", "[]");
            }
            #[test]
            fn project_limits_not_a_mapping() {
                project_bad("factory.limits", "90");
            }
            #[test]
            fn project_minutes_not_an_integer() {
                project_bad("factory.limits.minutes", "\"ninety\"");
            }
            #[test]
            fn project_minutes_zero() {
                project_bad("factory.limits.minutes", "0");
            }
            #[test]
            fn project_minutes_negative() {
                project_bad("factory.limits.minutes", "-5");
            }
            #[test]
            fn project_minutes_fractional() {
                project_bad("factory.limits.minutes", "1.5");
            }
            #[test]
            fn project_minutes_written_as_90_0_is_fractional() {
                let errs = project_err(&PROJECT.replace("minutes: 90", "minutes: 90.0"));
                assert_one(&errs, "factory.limits.minutes");
                assert_eq!(errs[0].message, "must be an integer of at least 1");
            }
            #[test]
            fn project_gate_retries_not_an_integer() {
                project_bad("factory.limits.gate_retries", "many");
            }
            #[test]
            fn project_gate_retries_negative() {
                project_bad("factory.limits.gate_retries", "-1");
            }
            #[test]
            fn project_autonomy_merge_is_rejected_with_phase_note() {
                let errs = project_err(&edit(PROJECT, |tree| {
                    set(tree, "factory.autonomy", "merge")
                }));
                assert_one(&errs, "factory.autonomy");
                assert_eq!(
                    errs[0].message,
                    "unsupported value `merge`; only `pr` is accepted in phase 1"
                );
            }
            #[test]
            fn project_autonomy_deploy_is_rejected() {
                project_bad("factory.autonomy", "deploy");
            }
            #[test]
            fn project_autonomy_not_a_string() {
                let errs = project_err(&edit(PROJECT, |tree| set(tree, "factory.autonomy", "7")));
                assert_one(&errs, "factory.autonomy");
                assert_eq!(
                    errs[0].message,
                    "must be a string; only `pr` is accepted in phase 1"
                );
            }
            #[test]
            fn project_key_present_with_null_is_a_type_error_not_absent() {
                let errs = project_err(&edit(PROJECT, |tree| set(tree, "factory.base", "null")));
                assert_one(&errs, "factory.base");
                assert_eq!(errs[0].message, "must be a string");
            }
        }

        #[test]
        fn project_reports_all_errors_not_just_the_first() {
            let t = edit(PROJECT, |tree| {
                remove(tree, "factory.base");
                set(tree, "factory.limits.minutes", "0");
                set(tree, "factory.autonomy", "merge");
            });
            let errs = project_err(&t);
            assert_eq!(errs.len(), 3);
            assert_eq!(
                fields(&errs),
                ["factory.base", "factory.limits.minutes", "factory.autonomy"]
            );
        }

        #[test]
        fn project_invalid_yaml_names_the_file() {
            assert_file_level(&project_err("factory: [unclosed\n"), "not valid YAML");
        }

        #[test]
        fn project_document_not_a_mapping() {
            assert_file_level(&project_err("- a\n- b\n"), "top level must be a mapping");
            assert_file_level(&project_err(""), "top level must be a mapping");
            assert_file_level(&project_err("5\n"), "top level must be a mapping");
        }
    }

    mod format_config_error {
        use super::*;

        #[test]
        fn names_the_file_the_field_and_the_message() {
            let e = ConfigError {
                file: "/m/c.yml".into(),
                field: Some("model".into()),
                message: "is missing".into(),
            };
            assert_eq!(e.to_string(), "/m/c.yml: model: is missing");
        }

        #[test]
        fn leaves_the_field_out_of_a_file_level_error() {
            let e = ConfigError {
                file: "/m/c.yml".into(),
                field: None,
                message: "top level must be a mapping".into(),
            };
            assert_eq!(e.to_string(), "/m/c.yml: top level must be a mapping");
        }
    }
}
