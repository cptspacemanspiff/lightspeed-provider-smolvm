//! Keep Lightspeed credential values out of the guest.
//!
//! Lightspeed resolves an environment's credential bindings on every
//! `process/start` and `job/start` and sends the real values in the dedicated
//! `secretEnv` field. Those frames pass through this provider's relay, so the
//! provider removes each value with a configured rule and hands it to smolvm's
//! credential substitution instead. The process sees smolvm's placeholder,
//! which envd's own environment carries and its children inherit, and the
//! real value is only added to HTTPS request headers sent to the rule's hosts.
//!
//! Frames are edited as JSON values, not typed structs, so fields this
//! provider does not know about pass through unchanged.

use std::collections::BTreeMap;

use environment_protocol::{
    data::methods::{JOB_START_METHOD, PROCESS_START_METHOD},
    error::{EnvironmentProtocolError, EnvironmentProtocolErrorCode},
    shared::SecretString,
};
use serde_json::{Map, Value, json};

use crate::config::{CredentialRule, UnknownSecretPolicy};

const SECRET_ENV: &str = "secretEnv";

#[derive(Debug)]
pub enum Rewrite {
    /// Forward the original frame.
    Unchanged,
    /// Forward `frame` after supplying `captured` to the machine.
    Rewritten {
        id: Value,
        frame: String,
        captured: BTreeMap<String, SecretString>,
    },
    /// Do not forward; answer Lightspeed with `response`.
    Rejected { response: String },
}

pub fn rewrite_frame(
    text: &str,
    rules: &BTreeMap<String, CredentialRule>,
    unknown: UnknownSecretPolicy,
) -> Rewrite {
    // Most frames are file and process I/O, some of them large; skip
    // parsing anything that cannot carry secrets.
    if !text.contains(SECRET_ENV) {
        return Rewrite::Unchanged;
    }
    let Ok(mut request) = serde_json::from_str::<Value>(text) else {
        return Rewrite::Unchanged;
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut captured = BTreeMap::new();
    let outcome = match method {
        PROCESS_START_METHOD => match request.get_mut("params") {
            Some(params) => strip(params, rules, unknown, &mut captured),
            None => Ok(false),
        },
        JOB_START_METHOD => match request
            .pointer_mut("/params/jobs")
            .and_then(Value::as_array_mut)
        {
            Some(jobs) => {
                let mut changed = false;
                let mut result = Ok(());
                for job in jobs {
                    match strip(job, rules, unknown, &mut captured) {
                        Ok(job_changed) => changed |= job_changed,
                        Err(message) => {
                            result = Err(message);
                            break;
                        }
                    }
                }
                result.map(|()| changed)
            }
            None => Ok(false),
        },
        _ => Ok(false),
    };
    match outcome {
        Ok(false) => Rewrite::Unchanged,
        Ok(true) => Rewrite::Rewritten {
            id,
            frame: request.to_string(),
            captured,
        },
        Err(message) => Rewrite::Rejected {
            response: error_response(id, EnvironmentProtocolErrorCode::Forbidden, message),
        },
    }
}

pub fn error_response(id: Value, code: EnvironmentProtocolErrorCode, message: String) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": EnvironmentProtocolError::new(code, message)})
        .to_string()
}

/// Remove ruled entries from `holder.secretEnv`. Returns whether anything
/// was removed; errors name the entry that blocks the request.
fn strip(
    holder: &mut Value,
    rules: &BTreeMap<String, CredentialRule>,
    unknown: UnknownSecretPolicy,
    captured: &mut BTreeMap<String, SecretString>,
) -> Result<bool, String> {
    let Some(object) = holder.as_object_mut() else {
        return Ok(false);
    };
    let Some(secret_env) = object.get_mut(SECRET_ENV).and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let mut kept = Map::new();
    let mut changed = false;
    for (name, value) in std::mem::take(secret_env) {
        if !rules.contains_key(&name) {
            if unknown == UnknownSecretPolicy::Reject {
                return Err(format!(
                    "secretEnv {name} has no credential rule on this provider; \
                     refusing to place the secret in the machine"
                ));
            }
            kept.insert(name, value);
            continue;
        }
        let Some(value) = value.as_str() else {
            return Err(format!("secretEnv {name} must be a string"));
        };
        // One machine-wide substitution per name: jobs in a single start
        // cannot carry different values for it.
        if captured
            .get(&name)
            .is_some_and(|existing| existing.expose() != value)
        {
            return Err(format!(
                "secretEnv {name} has conflicting values in one request"
            ));
        }
        captured.insert(name, SecretString::new(value));
        changed = true;
    }
    if kept.is_empty() {
        object.remove(SECRET_ENV);
    } else {
        object.insert(SECRET_ENV.to_owned(), Value::Object(kept));
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> BTreeMap<String, CredentialRule> {
        BTreeMap::from([(
            "GITHUB_TOKEN".to_owned(),
            CredentialRule {
                env_name: "GITHUB_TOKEN".to_owned(),
                hosts: vec!["api.github.com".to_owned()],
            },
        )])
    }

    fn process_start(secret_env: Value) -> String {
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "process/start",
            "params": {
                "processId": "p1",
                "argv": ["git", "fetch"],
                "env": {"GIT_TRACE": "0"},
                "secretEnv": secret_env,
                "futureField": true
            }
        })
        .to_string()
    }

    #[test]
    fn frames_without_secrets_are_not_parsed_or_changed() {
        let frame = json!({"jsonrpc":"2.0","id":1,"method":"fs/readFile","params":{"path":"/x"}})
            .to_string();
        assert!(matches!(
            rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject),
            Rewrite::Unchanged
        ));
    }

    #[test]
    fn ruled_secrets_are_stripped_and_captured() {
        let frame = process_start(json!({"GITHUB_TOKEN": "ghp_real"}));
        let Rewrite::Rewritten {
            id,
            frame,
            captured,
        } = rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject)
        else {
            panic!("expected rewrite");
        };
        assert_eq!(id, json!(7));
        assert_eq!(captured["GITHUB_TOKEN"].expose(), "ghp_real");
        assert!(!frame.contains("ghp_real"));
        let forwarded: Value = serde_json::from_str(&frame).unwrap();
        assert!(forwarded["params"].get("secretEnv").is_none());
        assert_eq!(forwarded["params"]["env"]["GIT_TRACE"], "0");
        assert_eq!(forwarded["params"]["futureField"], true);
    }

    #[test]
    fn unruled_secrets_are_refused_by_default() {
        let frame = process_start(json!({"GITHUB_TOKEN": "ghp_real", "AWS_SECRET": "s3cr3t"}));
        let Rewrite::Rejected { response } =
            rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject)
        else {
            panic!("expected rejection");
        };
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], "forbidden");
        assert!(!response.to_string().contains("s3cr3t"));
    }

    #[test]
    fn pass_through_keeps_unruled_secrets_in_the_frame() {
        let frame = process_start(json!({"GITHUB_TOKEN": "ghp_real", "OTHER": "kept"}));
        let Rewrite::Rewritten { frame, .. } =
            rewrite_frame(&frame, &rules(), UnknownSecretPolicy::PassThrough)
        else {
            panic!("expected rewrite");
        };
        let forwarded: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(forwarded["params"]["secretEnv"], json!({"OTHER": "kept"}));
    }

    #[test]
    fn job_starts_are_rewritten_per_job() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": "j",
            "method": "job/start",
            "params": {
                "namespace": "n",
                "requestId": "r",
                "jobs": [
                    {"jobId": "a", "argv": ["true"], "secretEnv": {"GITHUB_TOKEN": "ghp_real"}},
                    {"jobId": "b", "argv": ["true"]}
                ]
            }
        })
        .to_string();
        let Rewrite::Rewritten {
            frame, captured, ..
        } = rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject)
        else {
            panic!("expected rewrite");
        };
        assert_eq!(captured.len(), 1);
        assert!(!frame.contains("ghp_real"));
    }

    #[test]
    fn conflicting_values_across_jobs_are_refused() {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "job/start",
            "params": {"namespace": "n", "requestId": "r", "jobs": [
                {"jobId": "a", "argv": ["true"], "secretEnv": {"GITHUB_TOKEN": "one"}},
                {"jobId": "b", "argv": ["true"], "secretEnv": {"GITHUB_TOKEN": "two"}}
            ]}
        })
        .to_string();
        assert!(matches!(
            rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject),
            Rewrite::Rejected { .. }
        ));
    }

    #[test]
    fn other_methods_mentioning_secret_env_are_untouched() {
        let frame = json!({"jsonrpc":"2.0","id":1,"method":"fs/writeFile",
            "params":{"path":"/notes.txt","content":"secretEnv"}})
        .to_string();
        assert!(matches!(
            rewrite_frame(&frame, &rules(), UnknownSecretPolicy::Reject),
            Rewrite::Unchanged
        ));
    }
}
