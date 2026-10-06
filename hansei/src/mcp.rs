// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Agent queries over the same cached rows and analysis as the REPL.
//! The launch command chooses the core; tools cannot open arbitrary
//! files, capture a live process, write a bundle, or execute commands.

mod protocol;

use crate::{Session, SessionArgs, output::Theme, tasks, threads};
use anyhow::{Result, bail, ensure};
use hansei_runtime::tokio::{assess::WaitAssessment, contract};
use proc::Target;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Literal, case-insensitive substring; no regex or command syntax.
    contains: Option<String>,
    state: Option<String>,
}

impl Query {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_LIMIT).contains(&self.limit),
            "limit must be between 1 and {MAX_LIMIT}"
        );
        ensure!(
            self.contains.as_ref().is_none_or(|s| s.len() <= 256),
            "contains exceeds 256 bytes"
        );
        ensure!(
            self.state.as_ref().is_none_or(|s| s.len() <= 64),
            "state exceeds 64 bytes"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskQuery {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceQuery {
    task_id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

fn catalog() -> Value {
    let limit =
        json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT});
    let offset = json!({"type": "integer", "minimum": 0, "default": 0});
    let task_id = json!({"type": "string", "minLength": 1, "maxLength": 64,
        "description": "Exact id string returned by hansei_tasks (decimal ID or hexadecimal header address)."});
    let page = json!({"offset": offset, "limit": limit,
        "contains": {"type": "string", "maxLength": 256, "description": "Case-insensitive literal substring of the task's future/wait/site, or the thread's name/role/top frame."},
        "state": {"type": "string", "maxLength": 64, "description": "Exact task state, including any cancelled suffix, or exact thread role_kind. Discover values with hansei_summary."}});
    Value::Array([
        ("hansei_summary", "Start here: core identity, runtime and task/thread counts, and analysis diagnostics. This is one frozen snapshot, not a liveness monitor.", object(json!({}), &[])),
        ("hansei_tasks", "Filter and page structured task rows. IDs are lossless strings; next_offset continues the same filters. Idle does not mean deadlocked.", object(page.clone(), &[])),
        ("hansei_task", "Inspect one task, including its stable assessment kind and evidence notes. Only verified_wait establishes a saved resource dependency; wait_set is disjunctive.", object(json!({"task_id": task_id}), &["task_id"])),
        ("hansei_threads", "Filter and page structured OS thread rows. state selects role_kind; contains searches name, role and top frame.", object(page, &[])),
        ("hansei_trace", "Page a task's structured async stack, leaf first. Includes source locations and the reason the walk stopped. Running tasks may have torn state and stop at the root. Does not expose local values.", object(json!({"task_id": task_id, "offset": offset, "limit": limit}), &["task_id"])),
        ("hansei_graph", "Read the existing wait graph and conditional polling-barrier evidence as bounded text. Absence of an edge is not proof of absence of a dependency. Output may be truncated; narrow investigation using task tools.", object(json!({"limit": limit}), &[])),
    ].into_iter().map(|(name, description, input)| json!({
        "name": name, "description": description, "inputSchema": input,
        "outputSchema": object(json!({"schema_version": {"const": 1}, "data": {"type": "object"}, "diagnostics": {"type": "object"}}), &["schema_version", "data", "diagnostics"]),
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
    })).collect())
}

fn page<T: serde::Serialize>(rows: Vec<T>, offset: usize, limit: usize) -> Result<Value> {
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_LIMIT}"
    );
    let total = rows.len();
    ensure!(
        offset <= total,
        "offset {offset} exceeds matching row count {total}"
    );
    let end = offset.saturating_add(limit).min(total);
    Ok(
        json!({"items": rows.into_iter().skip(offset).take(limit).collect::<Vec<_>>(),
        "total": total, "offset": offset, "next_offset": (end < total).then_some(end)}),
    )
}

fn assessment_kind(assessment: &WaitAssessment) -> &'static str {
    match assessment {
        WaitAssessment::NotWaiting(_) => "not_waiting",
        WaitAssessment::Runnable(_) => "runnable",
        WaitAssessment::Unresumed => "unresumed",
        WaitAssessment::ResourceReady(_) => "resource_ready",
        WaitAssessment::Waiting(_) => "verified_wait",
        WaitAssessment::Set(_) => "wait_set",
        WaitAssessment::NeverReady { .. } => "never_ready",
        WaitAssessment::Unknown(_) => "unknown",
    }
}

fn task_index<T: Target>(session: &Session<'_, T>, id: &str) -> Result<usize> {
    ensure!(
        !id.is_empty() && id.len() <= 64,
        "task_id must contain 1 to 64 bytes"
    );
    let mut matches = tasks::rows(session)
        .iter()
        .enumerate()
        .filter(|(_, row)| row.id == id);
    let Some((index, _)) = matches.next() else {
        bail!("No task {id:?}; use hansei_tasks to discover IDs");
    };
    ensure!(
        matches.next().is_none(),
        "Ambiguous task ID {id:?}; launch with --runtime to select one runtime"
    );
    Ok(index)
}

fn task<T: Target>(session: &Session<'_, T>, index: usize) -> Result<Value> {
    let mut row = serde_json::to_value(&tasks::rows(session)[index])?;
    let addr = session.tasks.tasks[index].addr;
    let wait = session
        .analysis()
        .waits
        .iter()
        .find(|w| w.task.addr == addr);
    row["address"] = json!(format!("{addr:?}"));
    row["assessment"] = match wait {
        Some(wait) => json!({"kind": assessment_kind(&wait.assessment),
            "continuation": format!("{:?}", wait.continuation),
            "notes": wait.notes, "held_capped": wait.held_capped,
            "verified_target": wait.verified().map(|v| v.target().line())}),
        None => {
            json!({"kind": "unavailable", "notes": ["No wait assessment was established for this task."]})
        }
    };
    Ok(row)
}

fn counts<'a>(values: impl Iterator<Item = &'a str>) -> BTreeMap<&'a str, usize> {
    let mut result = BTreeMap::new();
    for value in values {
        *result.entry(value).or_default() += 1;
    }
    result
}

fn call<T: Target>(session: &Session<'_, T>, name: &str, args: Value) -> Result<Value> {
    match name {
        "hansei_summary" => {
            let _: Empty = serde_json::from_value(args)?;
            Ok(json!({
                "core": session.core.display().to_string(),
                "type_source": session.bundle_source.to_string(),
                "rustc_version": session.bundle.meta.rustc_version,
                "tokio_version": session.bundle.meta.tokio_version.as_ref().map(ToString::to_string),
                "runtimes": session.runtimes.iter().enumerate().map(|(index, r)| json!({"index": index, "flavor": r.flavor.to_string()})).collect::<Vec<_>>(),
                "task_count": session.tasks.tasks.len(),
                "task_states": counts(tasks::rows(session).iter().map(|t| t.state.as_str())),
                "thread_count": session.lwps.len(),
                "thread_roles": counts(threads::rows(session).iter().map(|t| t.role_kind)),
                "wait_assessments": counts(session.analysis().waits.iter().map(|w| assessment_kind(&w.assessment))),
                "polling_barrier_count": session.analysis().barriers.len(),
            }))
        }
        "hansei_tasks" | "hansei_threads" => {
            let q: Query = serde_json::from_value(args)?;
            q.validate()?;
            let needle = q.contains.as_deref().unwrap_or("").to_lowercase();
            if name == "hansei_tasks" {
                let rows: Vec<_> = tasks::rows(session)
                    .iter()
                    .filter(|row| {
                        q.state.as_ref().is_none_or(|s| s == &row.state)
                            && format!(
                                "{} {} {}",
                                row.future,
                                row.wait_text,
                                row.awaiting_at.as_deref().unwrap_or("")
                            )
                            .to_lowercase()
                            .contains(&needle)
                    })
                    .collect();
                page(rows, q.offset, q.limit)
            } else {
                let rows: Vec<_> = threads::rows(session)
                    .iter()
                    .filter(|row| {
                        q.state.as_ref().is_none_or(|s| s == row.role_kind)
                            && format!(
                                "{} {} {}",
                                row.name.as_deref().unwrap_or(""),
                                row.role,
                                row.frame0.as_deref().unwrap_or("")
                            )
                            .to_lowercase()
                            .contains(&needle)
                    })
                    .collect();
                let mut result = page(rows, q.offset, q.limit)?;
                for row in result["items"].as_array_mut().unwrap() {
                    // Tokio IDs can exceed JavaScript's exact integer range.
                    row["task"] = json!(row["task"].as_u64().map(|id| id.to_string()));
                }
                Ok(result)
            }
        }
        "hansei_task" => {
            let q: TaskQuery = serde_json::from_value(args)?;
            task(session, task_index(session, &q.task_id)?)
        }
        "hansei_trace" => {
            let q: TraceQuery = serde_json::from_value(args)?;
            ensure!(
                (1..=MAX_LIMIT).contains(&q.limit),
                "limit must be between 1 and {MAX_LIMIT}"
            );
            let index = task_index(session, &q.task_id)?;
            let inspection = session
                .read_with(|read| session.ctx.inspect_task(&session.tasks.tasks[index], read))?;
            let (frames, end) = match inspection {
                None => (Vec::new(), "No resident root future".to_owned()),
                Some(inspection) => {
                    let frames: Vec<Value> = inspection.chain.frames.iter().rev().enumerate().map(|(frame, f)| json!({
                        "frame": frame, "address": format!("{:#x}", f.future.addr),
                        "type": f.future.ty.name(), "dynamic_symbol": f.dyn_symbol,
                        "state": f.state.as_ref().map(|s| s.name),
                        "awaiting_at": f.state.as_ref().and_then(|s| s.await_loc).map(|(file, line)| json!({"file": file, "line": line}))
                    })).collect();
                    (frames, format!("{:?}", inspection.chain.end))
                }
            };
            let mut result = page(frames, q.offset, q.limit)?;
            result["task_id"] = json!(q.task_id);
            result["chain_end"] = json!(end);
            result["assessment"] = task(session, index)?["assessment"].clone();
            Ok(result)
        }
        "hansei_graph" => {
            let q: GraphQuery = serde_json::from_value(args)?;
            ensure!(
                (1..=MAX_LIMIT).contains(&q.limit),
                "limit must be between 1 and {MAX_LIMIT}"
            );
            let mut out = BoundedText::default();
            crate::graph::exec_graph(session, Some(q.limit), Theme::plain(), &mut out)?;
            Ok(
                json!({"text": String::from_utf8_lossy(&out.bytes), "byte_truncated": out.truncated,
                "row_limit": q.limit, "format": "hansei_text", "note": "The graph is a partial view of supported relations; an empty graph does not establish absence of deadlock."}),
            )
        }
        _ => bail!("Unknown tool {name:?}"),
    }
}

#[derive(Default)]
struct BoundedText {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Write for BoundedText {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let keep = bytes
            .len()
            .min((32 * 1024_usize).saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..keep]);
        self.truncated |= keep < bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn diagnostics<T: Target>(session: &Session<'_, T>, args: &SessionArgs) -> Value {
    let census = session.census();
    let mut warnings: Vec<String> = session.ctx.contract_report().degraded(session.policy);
    warnings.extend(crate::unsupported_warning(&session.bundle.meta));
    warnings.extend(contract::version_ceiling_notice(&session.bundle.meta).map(|s| s.to_string()));
    if args.force {
        warnings.push("--force was set: artifact identity checks may have been bypassed.".into());
    }
    if args.best_effort {
        warnings.push("--best-effort was set: some walk paths may be degraded.".into());
    }
    if matches!(
        session.bundle.meta.vtable_data,
        hansei_bundle::VtableDataSource::None
    ) {
        warnings
            .push("No vtable program contents: dyn trait-object coverage is incomplete.".into());
    }
    warnings.extend(
        session
            .tasks
            .errors
            .iter()
            .chain(&session.analysis().errors)
            .chain(&census.errors)
            .map(|e| format!("{e:#}")),
    );
    let total = warnings.len();
    warnings.truncate(20);
    json!({"warnings": warnings, "omitted_warnings": total.saturating_sub(20),
        "census_capped": {"deep": census.capped.deep, "distant": census.capped.distant, "unavailable": census.capped.unavailable},
        "uncertain_futures": census.uncertain, "refused_futures": census.refused,
        "scope": "Frozen core; unsupported or torn state remains unknown. Additional native capture/unwind diagnostics are written to stderr."})
}

pub(crate) fn run<T: Target>(session: &Session<'_, T>, args: &SessionArgs) -> Result<()> {
    let diagnostics = diagnostics(session, args);
    protocol::serve(
        io::stdin().lock(),
        io::stdout().lock(),
        catalog(),
        |name, args| {
            Ok(
                json!({"schema_version": 1, "data": call(session, name, args)?, "diagnostics": diagnostics}),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_bounded_and_offsets_are_explicit() {
        let rows = || -> Vec<Value> { (0..5).map(|id| json!({"id": id.to_string()})).collect() };
        let first = page(rows(), 0, 2).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        assert_eq!(first["next_offset"], 2);
        let last = page(rows(), 4, 2).unwrap();
        assert_eq!(last["items"][0]["id"], "4");
        assert!(last["next_offset"].is_null());
        assert!(page(rows(), 6, 2).is_err());
        assert!(page(rows(), 0, 0).is_err());
        assert!(page(rows(), 0, 101).is_err());
        assert_eq!(page(rows(), 5, 2).unwrap()["items"], json!([]));
    }

    #[test]
    fn rejects_command_and_path_arguments() {
        assert!(serde_json::from_value::<Empty>(json!({"core": "/etc/passwd"})).is_err());
        assert!(serde_json::from_value::<TaskQuery>(json!({"task_id": 42})).is_err());
        assert!(serde_json::from_value::<Query>(json!({"exec": "save-tokio-info x"})).is_err());
        assert!(serde_json::from_value::<Query>(json!({"offset": -1})).is_err());
        assert!(
            serde_json::from_value::<Query>(json!({"contains": "x".repeat(257)}))
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn graph_output_has_a_byte_bound() {
        let mut out = BoundedText::default();
        out.write_all(&vec![b'x'; 100_000]).unwrap();
        assert_eq!(out.bytes.len(), 32 * 1024);
        assert!(out.truncated);
    }
}
