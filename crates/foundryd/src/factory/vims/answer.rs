//! What `vims … --json` answers, read leniently.
//!
//! P2c's checks saw a few of these answers, and gave the field names of fewer still: an
//! operation's exit code and its `HOST_COMMAND_TIMEOUT` error, and a workspace's
//! `versions.base.variantDigest`. Every other field name here is a guess, named where it is
//! read, and each reader takes the spellings a JSON command line commonly uses (`camelCase`,
//! `snake_case`, the object at the top or under its own name). A reader that finds no field it
//! needs gives `None`, which the call reads as an answer it cannot read.

use serde_json::Value;

/// One operation of Vims: a call that makes or changes something, or a command `exec` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    /// Vims's id of the operation.
    pub id: String,
    /// Where it is.
    pub state: OpState,
    /// The exit code of an `exec` command that ran to its end.
    pub exit_code: Option<i32>,
    /// Vims's error code for an operation that failed, such as `HOST_COMMAND_TIMEOUT` for a
    /// command that ran past its limit.
    pub error: Option<String>,
    /// The workspace the operation made or acts on, as Vims names it (`wsp_…`).
    pub workspace: Option<String>,
}

/// Where an operation is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpState {
    /// Taken and not yet ended: queued, pending or running.
    Running,
    /// Ended as asked. An `exec` that finished has its exit code, whatever it is.
    Finished,
    /// Ended without doing what it was asked, with its error.
    Failed,
    /// Cancelled.
    Cancelled,
    /// A state no other variant names, lowercased.
    Other(String),
}

impl OpState {
    // Guessed spellings: P2c names none of these states.
    fn read(status: &str) -> Self {
        let status = status.to_ascii_lowercase();
        match status.as_str() {
            "queued" | "pending" | "accepted" | "running" | "started" | "in_progress" => {
                OpState::Running
            }
            "succeeded" | "success" | "completed" | "complete" | "done" | "finished" => {
                OpState::Finished
            }
            "failed" | "fail" | "error" | "errored" => OpState::Failed,
            "cancelled" | "canceled" => OpState::Cancelled,
            _ => OpState::Other(status),
        }
    }
}

impl Operation {
    /// The operation in `value`, at the top or under `operation`.
    fn from_value(value: &Value) -> Option<Self> {
        let value = inner(value, "operation");
        // Guessed: `id`; `exitCode` beside the state or in its `result`; `error.code`.
        let id = text(value, &[&["id"], &["operationId"], &["operation_id"]])?;
        let state = OpState::read(&text(value, &[&["status"], &["state"]]).unwrap_or_default());
        let exit_code = first(
            value,
            &[
                &["exitCode"],
                &["exit_code"],
                &["result", "exitCode"],
                &["result", "exit_code"],
            ],
        )
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
        let error = text(
            value,
            &[
                &["error", "code"],
                &["errorCode"],
                &["error_code"],
                &["result", "error", "code"],
                &["error"],
            ],
        );
        let workspace = text(
            value,
            &[
                &["workspaceId"],
                &["workspace_id"],
                &["workspace", "id"],
                &["workspace"],
                &["resourceId"],
                &["result", "workspaceId"],
                &["result", "workspace", "id"],
            ],
        );
        Some(Operation {
            id,
            state,
            exit_code,
            error,
            workspace,
        })
    }

    /// The operation `stdout` answers.
    pub(super) fn read(stdout: &str) -> Option<Self> {
        Self::from_value(&json(stdout)?)
    }

    /// The operations `operation list` answers: an array, or one under `operations`.
    pub(super) fn read_list(stdout: &str) -> Option<Vec<Self>> {
        let value = json(stdout)?;
        // Guessed: the list at the top, or under `operations`, `items` or `data`.
        let list = match &value {
            Value::Array(list) => list,
            _ => first(&value, &[&["operations"], &["items"], &["data"]])?.as_array()?,
        };
        list.iter().map(Self::from_value).collect()
    }
}

/// What a call that changes a workspace answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// The operation Vims made for it, to be read with `operation show`.
    Operation(Operation),
    /// The workspace, whose state says whether the change has happened.
    Workspace(Workspace),
    /// Nothing: an empty answer, or an empty object.
    Done,
}

impl Change {
    /// The change `stdout` answers. An answer that is an error, or names neither an operation
    /// nor a workspace, cannot be read, so the caller finds out by the call's key rather than
    /// take the change as made.
    pub(super) fn read(stdout: &str) -> Option<Self> {
        Self::read_or(stdout, None)
    }

    /// As [`Change::read`], for a change whose answer may be the thing it made, such as a
    /// grant: an answer that is no error and names neither an operation nor a workspace is
    /// the change made. Guessed: P2c records no grant's answer.
    pub(super) fn read_made(stdout: &str) -> Option<Self> {
        Self::read_or(stdout, Some(Change::Done))
    }

    /// The change `stdout` answers, or `otherwise` for an answer that is no error and names
    /// neither an operation nor a workspace.
    fn read_or(stdout: &str, otherwise: Option<Self>) -> Option<Self> {
        if stdout.trim().is_empty() {
            return Some(Change::Done);
        }
        let value = json(stdout)?;
        let fields = value.as_object()?;
        if fields.is_empty() {
            return Some(Change::Done);
        }
        // An error with no state of its own is the answer's error, not an operation's.
        let failed = fields.get("error").is_some_and(|error| !error.is_null());
        let state = text(&value, &[&["status"], &["state"]]).map(|status| OpState::read(&status));
        if failed && state.is_none() {
            return None;
        }
        // Guessed: an operation is under `operation`, or is the answer when it has an
        // operation id, or an `id` that is no workspace's (`wsp_`, which the checks saw); a
        // workspace is under `workspace`, or is the answer when its `id` is a workspace's. Where
        // the answer may be the thing made, a bare `id` is an operation's only with a state an
        // operation has.
        let made = otherwise.is_some();
        let id = text(&value, &[&["id"]]);
        let operation_state = state
            .as_ref()
            .is_some_and(|state| !matches!(state, OpState::Other(_)));
        let operation = value.get("operation").is_some_and(Value::is_object)
            || text(&value, &[&["operationId"], &["operation_id"]]).is_some()
            || id
                .as_deref()
                .is_some_and(|id| !id.starts_with("wsp_") && (operation_state || !made));
        if operation {
            return Operation::from_value(&value).map(Change::Operation);
        }
        let workspace = value.get("workspace").is_some_and(Value::is_object)
            || id.as_deref().is_some_and(|id| id.starts_with("wsp_"));
        if workspace {
            return Workspace::from_value(&value).map(Change::Workspace);
        }
        // An answer whose own state says it failed is never the change made.
        let said_failed = matches!(state, Some(OpState::Failed | OpState::Cancelled));
        if failed || said_failed {
            None
        } else {
            otherwise
        }
    }
}

/// The output `operation logs` answers: the text, or the text under `logs` or `output`.
pub(super) fn read_logs(stdout: &str) -> String {
    // Guessed: plain text, a JSON string, or a string under `logs`, `output` or `text`.
    match json(stdout) {
        Some(Value::String(text)) => text,
        Some(value) => {
            text(&value, &[&["logs"], &["output"], &["text"]]).unwrap_or_else(|| stdout.to_owned())
        }
        None => stdout.to_owned(),
    }
}

/// One workspace, as `vims show` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    /// Vims's id (`wsp_…`).
    pub id: String,
    /// Its name, when Vims gives one.
    pub name: Option<String>,
    /// Whether it runs.
    pub state: WorkspaceState,
    /// The source key of the build it rests on, `golden:<digest>`.
    pub source_key: Option<String>,
}

/// Whether a workspace runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceState {
    Running,
    /// Put to sleep with `vims pause`.
    Paused,
    /// Ended with `vims stop`: its files kept, its compute gone.
    Stopped,
    /// Put to sleep by Vims, for being idle or for want of credit.
    Asleep,
    /// A state no other variant names, lowercased.
    Other(String),
}

impl WorkspaceState {
    // Guessed spellings, but `Stopped` and `Paused`, which the checks saw.
    fn read(status: &str) -> Self {
        let status = status.to_ascii_lowercase();
        match status.as_str() {
            "running" => WorkspaceState::Running,
            "paused" => WorkspaceState::Paused,
            "stopped" => WorkspaceState::Stopped,
            "asleep" | "sleeping" | "idle" => WorkspaceState::Asleep,
            _ => WorkspaceState::Other(status),
        }
    }
}

impl Workspace {
    pub(super) fn read(stdout: &str) -> Option<Self> {
        Self::from_value(&json(stdout)?)
    }

    /// The workspace in `value`, at the top or under `workspace`.
    fn from_value(value: &Value) -> Option<Self> {
        let value = inner(value, "workspace");
        let id = text(value, &[&["id"], &["workspaceId"]])?;
        // Seen in check 1: `versions.base.variantDigest` is `sha256:<digest>` for the same digest
        // as the template's `golden:<digest>` source key.
        let source_key = text(value, &[&["versions", "base", "variantDigest"]]).map(|digest| {
            let digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
            format!("golden:{digest}")
        });
        Some(Workspace {
            id,
            name: text(value, &[&["name"]]),
            state: WorkspaceState::read(
                &text(value, &[&["status"], &["state"]]).unwrap_or_default(),
            ),
            source_key,
        })
    }
}

/// One template, as `vims templates show` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub name: String,
    /// The source key of the build a new workspace is made from, `golden:<digest>`.
    pub source_key: Option<String>,
    /// The repos the template checks out, each with its path in a workspace.
    pub sources: Vec<TemplateSource>,
    /// The vCPU of its machine.
    pub vcpu: Option<u32>,
    pub retired: bool,
}

/// A repo a template checks out, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateSource {
    /// The repo, as Vims names it.
    pub repo: Option<String>,
    /// The checkout's path in a workspace, with its leading `/`.
    pub path: String,
}

impl Template {
    pub(super) fn read(stdout: &str) -> Option<Self> {
        let value = json(stdout)?;
        let value = inner(&value, "template");
        let name = text(value, &[&["name"]])?;
        // Guessed: the key under `sourceKey`; failing that, the one `golden:` value the answer
        // holds, and none when it holds two, since one may be an older build kept as a set-back.
        let source_key = text(
            value,
            &[&["sourceKey"], &["source_key"], &["source", "key"]],
        )
        .or_else(|| {
            let mut keys = Vec::new();
            golden_keys(value, &mut keys);
            keys.dedup();
            match keys.as_slice() {
                [one] => Some(one.clone()),
                _ => None,
            }
        });
        // Guessed: `sources` (or `repos`, `repositories`), each with `repo` and `path`.
        let sources = first(value, &[&["sources"], &["repos"], &["repositories"]])
            .and_then(Value::as_array)
            .map(|sources| {
                sources
                    .iter()
                    .filter_map(|source| {
                        Some(TemplateSource {
                            repo: text(
                                source,
                                &[&["repo"], &["repository"], &["fullName"], &["url"]],
                            ),
                            path: text(source, &[&["path"], &["sourcePath"], &["mountPath"]])?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let vcpu = first(
            value,
            &[
                &["vcpu"],
                &["vcpus"],
                &["machine", "vcpu"],
                &["resources", "vcpu"],
            ],
        )
        .and_then(Value::as_u64)
        .and_then(|vcpu| u32::try_from(vcpu).ok());
        let retired = value.get("retired").and_then(Value::as_bool) == Some(true)
            || text(value, &[&["status"]])
                .is_some_and(|status| status.eq_ignore_ascii_case("retired"));
        Some(Template {
            name,
            source_key,
            sources,
            vcpu,
            retired,
        })
    }
}

/// Every string in `value` that starts `golden:`, in the order found.
fn golden_keys(value: &Value, keys: &mut Vec<String>) {
    match value {
        Value::String(text) if text.starts_with("golden:") => {
            if !keys.contains(text) {
                keys.push(text.clone());
            }
        }
        Value::Array(items) => items.iter().for_each(|item| golden_keys(item, keys)),
        Value::Object(fields) => fields.values().for_each(|item| golden_keys(item, keys)),
        _ => {}
    }
}

/// One connection of the Vims account, as `vims connection list` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The id a grant names.
    pub id: String,
    /// What it connects to, such as `github`.
    pub provider: Option<String>,
    pub name: Option<String>,
}

impl Connection {
    pub(super) fn read_list(stdout: &str) -> Option<Vec<Self>> {
        let value = json(stdout)?;
        // Guessed: the list at the top or under `connections`, each with `id`, `provider` and
        // `name`.
        let list = match &value {
            Value::Array(list) => list,
            _ => first(&value, &[&["connections"], &["items"], &["data"]])?.as_array()?,
        };
        list.iter()
            .map(|connection| {
                Some(Connection {
                    id: text(connection, &[&["id"]])?,
                    provider: text(connection, &[&["provider"], &["kind"], &["type"]]),
                    name: text(connection, &[&["name"], &["label"]]),
                })
            })
            .collect()
    }
}

/// The GitHub logins Vims acts as, as `vims github` gives them.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct GithubLinks {
    pub links: Vec<GithubLink>,
}

/// A GitHub login Vims acts as, and the owner (an organisation or a user) it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubLink {
    /// The owner, or `None` when Vims names the login for every owner.
    pub owner: Option<String>,
    pub login: String,
}

impl GithubLinks {
    /// The login Vims acts as for `owner`, matched without case: the one named for that owner,
    /// else one named for every owner.
    pub fn login_for(&self, owner: &str) -> Option<&str> {
        let named = self.links.iter().find(|link| {
            link.owner
                .as_deref()
                .is_some_and(|known| known.eq_ignore_ascii_case(owner))
        });
        named
            .or_else(|| self.links.iter().find(|link| link.owner.is_none()))
            .map(|link| link.login.as_str())
    }

    pub(super) fn read(stdout: &str) -> Option<Self> {
        let value = json(stdout).filter(|value| value.is_object() || value.is_array())?;
        // Guessed: a list at the top or under `installations`, `accounts`, `links` or
        // `organizations`, each with its owner under `account.login` (or `owner`, `org`,
        // `organization`) and the login under `login` (or `user.login`); and a `login` at the
        // top for every owner.
        let mut links = Vec::new();
        let list = match &value {
            Value::Array(list) => Some(list),
            _ => first(
                &value,
                &[
                    &["installations"],
                    &["accounts"],
                    &["links"],
                    &["organizations"],
                ],
            )
            .and_then(Value::as_array),
        };
        for item in list.into_iter().flatten() {
            if let Some(login) = text(item, &[&["login"], &["user", "login"], &["githubLogin"]]) {
                let owner = text(
                    item,
                    &[
                        &["account", "login"],
                        &["owner"],
                        &["org"],
                        &["organization"],
                        &["account"],
                    ],
                );
                links.push(GithubLink { owner, login });
            }
        }
        if links.is_empty()
            && let Some(login) = text(&value, &[&["login"], &["user", "login"]])
        {
            links.push(GithubLink { owner: None, login });
        }
        Some(GithubLinks { links })
    }
}

/// Whether the profile `vims profile show` gives holds the skill named `skill`.
pub(super) fn profile_has_skill(stdout: &str, skill: &str) -> Option<bool> {
    // Guessed: a `skills` list anywhere in the answer, of names or of objects with a `name`.
    fn found(value: &Value, skill: &str) -> bool {
        match value {
            Value::Object(fields) => fields.iter().any(|(name, item)| {
                (name == "skills"
                    && item.as_array().is_some_and(|skills| {
                        skills.iter().any(|entry| {
                            entry.as_str() == Some(skill)
                                || text(entry, &[&["name"], &["id"]]).as_deref() == Some(skill)
                        })
                    }))
                    || found(item, skill)
            }),
            Value::Array(items) => items.iter().any(|item| found(item, skill)),
            _ => false,
        }
    }
    Some(found(&json(stdout)?, skill))
}

/// `text` read as JSON, or `None`.
fn json(text: &str) -> Option<Value> {
    serde_json::from_str(text.trim()).ok()
}

/// `value`'s field `name` when that is an object, else `value`.
fn inner<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .get(name)
        .filter(|inner| inner.is_object())
        .unwrap_or(value)
}

/// The first of `paths` that leads to a value in `value`.
fn first<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    paths.iter().find_map(|path| {
        path.iter()
            .try_fold(value, |value, field| value.get(field))
            .filter(|found| !found.is_null())
    })
}

/// The first of `paths` that leads to a string that is not empty.
fn text(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths.iter().find_map(|path| {
        path.iter()
            .try_fold(value, |value, field| value.get(field))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}
