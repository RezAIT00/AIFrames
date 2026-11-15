//! Suggestions: edits an AI proposes for a person to approve (`suggest`), listed with
//! `list_suggestions` and applied or dismissed with `resolve_suggestion` (or in the window's
//! Suggestions panel).

use super::*;
use aiframes_core::{suggestion_ops, Suggestion};

fn suggestion_j(s: &Suggestion) -> J {
    let mut j = json!({"id": s.id, "label": s.label, "ops": s.ops.iter().map(|o| o.name()).collect::<Vec<_>>()});
    if !s.reason.is_empty() {
        j["reason"] = json!(s.reason);
    }
    if let Some(t) = s.at {
        j["at"] = json!(t3(t));
    }
    if !s.author.is_empty() {
        j["author"] = json!(s.author);
    }
    j
}

pub(crate) fn suggest(s: &Arc<Services>, args: &J) -> ToolResult {
    let label = req_str(args, "label")?.trim().to_string();
    let ops = parse_ops(args.get("ops").ok_or("pass 'ops': the edit to propose (the same ops edit takes)")?)?;
    let mut o = json!({"op": "add_suggestion", "label": label, "ops": ops});
    if let Some(r) = arg_str(args, "reason") {
        o["reason"] = json!(r);
    }
    if let Some(t) = arg_f64(args, "at")? {
        o["at"] = json!(t);
    }
    let op = crate::actions::op(o)?;
    let show = arg_bool(args, "show", true)?;
    let mut res = commit_with(s, args, &format!("Suggest: {}", short(&label, 60)), vec![op], |j| {
        j["next"] = json!("the person reviews it in the Review tab (or ask them; resolve_suggestion applies or dismisses it)");
    })?;
    if show && s.hub.has_ui() {
        // Bring the Review tab forward so the person sees what is waiting for them.
        super::view::request_panel(s, args, "review")?;
        res["shown"] = json!("review");
    }
    Ok(res)
}

pub(crate) fn list_suggestions(s: &Arc<Services>, args: &J) -> ToolResult {
    with_doc(s, args, |id, g| {
        let list: Vec<J> = g.doc.project.suggestions.iter().map(suggestion_j).collect();
        Ok(json!({"project": id, "pending": list.len(), "suggestions": list}))
    })
}

pub(crate) fn resolve_suggestion(s: &Arc<Services>, args: &J) -> ToolResult {
    let which = req_str(args, "suggestion")?.to_string();
    let action = arg_str(args, "action").unwrap_or("apply").to_string();
    if action != "apply" && action != "dismiss" {
        return Err(format!("action must be \"apply\" or \"dismiss\" (got \"{action}\")"));
    }
    let (ops, label) = with_doc(s, args, |_, g| {
        let p = &g.doc.project;
        let ids: Vec<Id> = if which == "all" { p.suggestions.iter().map(|x| x.id.clone()).collect() } else { vec![which.clone()] };
        if ids.is_empty() {
            return Err("there are no pending suggestions".into());
        }
        let mut ops = Vec::new();
        let mut labels = Vec::new();
        for id in &ids {
            let sg = p.suggestions.iter().find(|x| &x.id == id);
            labels.push(sg.map(|x| x.label.clone()).unwrap_or_else(|| id.clone()));
            if action == "apply" {
                ops.extend(suggestion_ops(p, id).map_err(|e| e.to_string())?);
            } else {
                if sg.is_none() {
                    suggestion_ops(p, id).map_err(|e| e.to_string())?;
                }
                ops.push(Op::RemoveSuggestion { suggestion: id.clone() });
            }
        }
        let verb = if action == "apply" { "Apply" } else { "Dismiss" };
        let label = if labels.len() == 1 { format!("{verb} suggestion: {}", short(&labels[0], 60)) } else { format!("{verb} {} suggestions", labels.len()) };
        Ok((ops, label))
    })?;
    commit(s, args, &label, ops).map_err(|e| {
        if action == "apply" {
            format!("{e} — the project changed since it was suggested; dismiss it (action \"dismiss\") or suggest it again")
        } else {
            e
        }
    })
}
