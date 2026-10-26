//! The scripted model of the process tests, over the Messages API, for
//! `testkit::fakes::FakeClaudeServer`. It answers from the tool results in
//! each request, never from a count of calls, so a request sent again after
//! a worker is killed gets the same reply.

use std::collections::HashMap;

use serde_json::{Value, json};
use testkit::fakes::message;

use super::{envelope_json, plate, propose_mass};

/// The successful results in a request, by tool, and how many calls it
/// holds.
struct Seen {
    results: HashMap<String, Value>,
    calls: usize,
}

impl Seen {
    fn of(request: &Value) -> Self {
        let mut tools: HashMap<String, String> = HashMap::new();
        let mut results: HashMap<String, Value> = HashMap::new();
        let mut calls = 0;
        let blocks = request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["content"].as_array().into_iter().flatten());
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_owned();
                    tools.insert(id, block["name"].as_str().unwrap_or_default().to_owned());
                    calls += 1;
                }
                Some("tool_result") if block["is_error"] != true => {
                    let tool = tools.get(block["tool_use_id"].as_str().unwrap_or_default());
                    let content = envelope_json(block["content"].as_str().unwrap_or_default());
                    if let (Some(tool), Some(content)) = (tool, content)
                        && content["status"] == "ok"
                    {
                        results.insert(tool.clone(), content);
                    }
                }
                _ => {}
            }
        }
        Self { results, calls }
    }

    fn use_tool(&self, name: &str, input: Value) -> Value {
        message(vec![json!({
            "type": "tool_use",
            "id": format!("toolu_{}", self.calls + 1),
            "name": name,
            "input": input
        })])
    }

    /// The next step of the plate script: create `plate`, measure it, then
    /// propose what `propose` makes of the measurement, then stop talking.
    fn next(&self, plate: Value, propose: impl Fn(&Value, &[String]) -> Value) -> Value {
        match (
            self.results.get("create_plate"),
            self.results.get("measure"),
            self.results.contains_key("sdr_propose"),
        ) {
            (None, _, _) => self.use_tool("create_plate", plate),
            (Some(created), None, _) => self.use_tool(
                "measure",
                json!({
                    "part": created["result"]["part"],
                    "density": { "value": 2.70, "unit": "g/cm3" }
                }),
            ),
            (Some(_), Some(measured), false) => {
                let evidence: Vec<String> = measured["evidence_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect();
                self.use_tool("sdr_propose", propose(&measured["result"], &evidence))
            }
            (Some(_), Some(_), true) => message(vec![json!({ "type": "text", "text": "Done." })]),
        }
    }
}

/// Creates the standard plate, measures it and proposes its mass as
/// `part.mass`, the one requirement of the test job.
pub fn mass_only(request: &Value) -> Value {
    Seen::of(request).next(plate(), |measured, evidence| {
        let mass = measured["mass"]["value"].as_f64().unwrap_or_default();
        propose_mass(mass, evidence, true)
    })
}

/// The dev benchmark's plate (`evals/benchmarks/dev/part_design_001`): a
/// 112 × 72 × 6 mm plate with the given holes, which meets R1–R4. It
/// proposes L, W and T from the measured bounding box and the measured
/// mass, all citing the measurement, as the final design.
pub fn benchmark_plate(request: &Value) -> Value {
    let plate = json!({
        "length_mm": 112,
        "width_mm": 72,
        "thickness_mm": 6,
        "hole_diameter_mm": 6.5,
        "hole_inset_mm": 10
    });
    Seen::of(request).next(plate, |measured, evidence| {
        let source = match evidence.first() {
            Some(e) => json!({ "kind": "tool_result", "evidence_id": e }),
            None => json!({ "kind": "user" }),
        };
        let box_ = &measured["bounding_box"];
        let parameters = [
            ("L", &box_["length"]),
            ("W", &box_["width"]),
            ("T", &box_["height"]),
            ("mass", &measured["mass"]),
        ];
        let operations: Vec<Value> = parameters
            .iter()
            .map(|(id, quantity)| {
                json!({
                    "op": "set_parameter",
                    "parameter": {
                        "id": id,
                        "value": quantity,
                        "source": source,
                        "evidence_ids": evidence
                    }
                })
            })
            .collect();
        json!({
            "reason": "Record the measured plate as the final design.",
            "final": true,
            "operations": operations
        })
    })
}
