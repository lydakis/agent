//! The plan a bot keeps where the person sees it: the `plan` tool Sonny
//! declares on it, each call the whole plan, drawn as a checklist.
use serde_json::{Value, json};

/// The name of the tool a bot sets its plan with.
pub const TOOL: &str = "plan";

/// The steps a `plan` call sets, none clearing the plan.
pub fn steps(arguments: &Value) -> Option<Vec<String>> {
    (arguments["steps"].as_array()?.iter())
        .map(|step| step.as_str().map(str::to_owned))
        .collect()
}

/// The `plan` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Set the plan the person sees: how many steps are done, the step you are on, and \
            the whole list. Use it for any task of three or more steps. Each call replaces the plan, so \
            send every step each time, in order, each starting with \"[x] \" (done), \"[>] \" (doing \
            now) or \"[ ] \" (to do). Call it when you start, when a step starts or ends, and when the \
            plan changes. Name a step for its outcome in a few words; at most 30 steps of 200 bytes, \
            grouping small ones. Once the work is done and said, or you turn to something the plan does \
            not describe, call it with no steps to clear it.",
        "parameters": {
            "type": "object",
            "properties": {
                "steps": {"type": "array", "items": {"type": "string"},
                    "description": "Every step, such as \"[>] Write the signer\"; none clears the plan."}
            },
            "required": ["steps"],
            "additionalProperties": false
        },
        "result": "Plan set."
    })
}
