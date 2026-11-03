//! EDG (Executable Decision Graph) — HarnessMM-style guarded decision
//! reuse for sigil (PoC, arXiv:2610.32813).
//!
//! The model authors a literal action bank alongside its current tool
//! call; after tool receipts arrive, the agent loop checks each node's
//! public guard against *observed* receipts only. An eligible node
//! executes through the normal ToolRegistry and **replaces the next
//! model call** (certified callback substitution). New semantic
//! decisions, unsupported evidence, or abstention return control to the
//! model.
//!
//! Literal closure: every node's `args` must be concrete JSON values —
//! no placeholders awaiting future feedback. That is enforced here at
//! declaration time (whole-tree closure check) and re-checked before
//! admission.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::tools::{Tool, ToolOutput};

/// A single stored decision node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EdgNode {
    /// Short label the controller cites in certificates.
    pub label: String,
    /// Tool name to execute (must exist in the registry).
    pub tool: String,
    /// Complete literal arguments (closure enforced).
    pub args: Value,
    /// Label of the predecessor node ("" = eligible immediately after
    /// the authoring turn's tool results land).
    pub pred: String,
    /// Public guard on observed receipts.
    /// `last_ok`  — the predecessor's tool result must not be an error.
    /// `file_exists:<path>` — the given file must exist at admission.
    pub guard: String,
}

impl EdgNode {
    /// Literal closure: arguments must be concrete JSON. The only
    /// string placeholder shape we reject is a sole `${...}`/`{{...}}`
    /// token; nested structures are walked.
    fn is_literal(v: &Value) -> bool {
        match v {
            Value::String(s) => {
                let t = s.trim();
                !(t.starts_with("${") && t.ends_with('}'))
                    && !(t.starts_with("{{") && t.ends_with("}}"))
            }
            Value::Array(a) => a.iter().all(Self::is_literal),
            Value::Object(m) => m.values().all(Self::is_literal),
            _ => true,
        }
    }

    fn closed(&self) -> bool {
        Self::is_literal(&self.args)
    }
}

/// The shared bank + stats the controller reads after tool turns.
#[derive(Default)]
pub struct EdgBank {
    inner: RwLock<BankInner>,
    /// Certified callback substitutions this process performed.
    pub bypasses: AtomicU64,
    /// Times a guard check rejected / no eligible node → fell back to model.
    pub abstentions: AtomicU64,
}

#[derive(Default)]
struct BankInner {
    /// Pending nodes keyed by label (insertion ordered via Vec).
    nodes: Vec<EdgNode>,
    /// Labels of nodes already executed (single-fire).
    done: Vec<String>,
    /// Labels whose failure should drop their dependents.
    failed: Vec<String>,
    /// Last observed tool receipt: (tool name, ok, output snippet).
    last_receipt: Option<(String, bool, String)>,
    /// Monotonic certificate counter.
    cert_seq: u64,
}

impl EdgBank {
    #[cfg(test)]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace the unresolved remainder with a freshly authored bank
    /// (already-executed receipts stay; the model re-authors on fallback).
    pub fn replace(&self, nodes: Vec<EdgNode>) -> usize {
        let mut b = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let keep_done = std::mem::take(&mut b.done);
        b.nodes = nodes;
        b.failed.clear();
        // Preserve completed history (episode receipts remain valid).
        b.done = keep_done;
        b.nodes.len()
    }

    /// Record an observed tool receipt (called by the agent loop after
    /// every tool turn, controller on or off — receipts are facts).
    pub fn observe(&self, tool: &str, ok: bool, output: &str) {
        let mut b = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let snippet: String = output.chars().take(200).collect();
        b.last_receipt = Some((tool.to_string(), ok, snippet));
    }

    /// Guard evaluation consumes observed receipts only — never model
    /// predictions. Returns the eligible node (single-fire) or None.
    ///
    /// Pred semantics (PoC, flat banks): a node is pred-complete when
    /// its predecessor label is in `done` (bypass-executed earlier in
    /// this episode). Nodes that follow the *authoring turn's own* tool
    /// call use `pred: ""` + a receipt guard (`last_ok` /
    /// `file_exists:`) — the receipt itself is the completion evidence.
    fn eligible(&self, workspace: Option<&std::path::Path>) -> Option<EdgNode> {
        let b = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if b.nodes.is_empty() {
            return None;
        }
        let (_tool, last_ok, _snippet) = b.last_receipt.as_ref()?;
        for n in &b.nodes {
            if b.done.contains(&n.label) || b.failed.contains(&n.label) {
                continue;
            }
            if !n.pred.is_empty() && !b.done.contains(&n.pred) {
                continue;
            }
            if !n.closed() {
                continue;
            }
            // Guard check against observed state only.
            let guard_ok = match n.guard.as_str() {
                "last_ok" => *last_ok,
                g if g.starts_with("file_exists:") => {
                    let p = g.trim_start_matches("file_exists:");
                    let full = match workspace {
                        Some(ws) if !p.starts_with('/') => ws.join(p),
                        _ => std::path::PathBuf::from(p),
                    };
                    full.exists()
                }
                "none" => true,
                _ => false, // unknown guard → defer to the model
            };
            if guard_ok {
                return Some(n.clone());
            }
        }
        None
    }

    /// Claim a node the controller just fetched via `next_eligible`:
    /// mark executed and mint the substitution certificate.
    pub fn claim(&self, node: &EdgNode) -> Certificate {
        let mut b = self.inner.write().unwrap_or_else(|e| e.into_inner());
        b.cert_seq += 1;
        b.done.push(node.label.clone());
        self.bypasses.fetch_add(1, Ordering::Relaxed);
        Certificate {
            seq: b.cert_seq,
            label: node.label.clone(),
            tool: node.tool.clone(),
            args: node.args.to_string(),
            guard: node.guard.clone(),
        }
    }

    /// Controller entry: find + claim the next eligible node.
    pub fn next_eligible(&self, workspace: Option<&std::path::Path>) -> Option<EdgNode> {
        self.eligible(workspace)
    }

    pub fn record_failure(&self, label: &str) {
        let mut b = self.inner.write().unwrap_or_else(|e| e.into_inner());
        b.failed.push(label.to_string());
        // Drop dependents of a failed node (they may never become ready).
        b.nodes.retain(|n| n.pred != label);
    }

    /// Stats for the run report: (certified bypasses, abstentions).
    #[allow(dead_code)]
    pub fn stats(&self) -> (u64, u64) {
        (
            self.bypasses.load(Ordering::Relaxed),
            self.abstentions.load(Ordering::Relaxed),
        )
    }
}

impl BankInner {
    #[allow(dead_code)]
    fn _unused(&self) {}
}

impl EdgNode {
    #[allow(dead_code)]
    fn _unused(&self) {}
}

/// Substitution certificate (CCS): binds the executed literal action to
/// the omitted model call. The stderr `[edg]` line IS the ledger.
#[derive(Debug, Clone)]
pub struct Certificate {
    pub seq: u64,
    pub label: String,
    pub tool: String,
    pub args: String,
    pub guard: String,
}

/// Built-in tool: the model declares an EDG bank (authoring).
pub struct DeclareEdgTool {
    pub bank: Arc<EdgBank>,
}

impl DeclareEdgTool {
    fn validate(&self, nodes: &[Value]) -> anyhow::Result<Vec<EdgNode>> {
        validate_nodes(nodes)
    }
}

#[async_trait]
impl Tool for DeclareEdgTool {
    fn name(&self) -> &str {
        "declare_edg"
    }

    fn description(&self) -> &str {
        "Declare an Executable Decision Graph: future tool actions whose \
         semantics and literal arguments are ALREADY fully resolved. Each \
         node: {label, tool, args (concrete JSON, no placeholders), pred \
         (label of the preceding node, '' for root), guard}. Guards: \
         'last_ok' (previous tool result was not an error) or \
         'file_exists:<path>'. When a later tool receipt satisfies a \
         guard, sigil executes the stored action DIRECTLY and skips the \
         model call. Only declare actions you have already decided — \
         e.g. after writing a script, declare the run command; after a \
         successful run, declare the verification command. New semantic \
         decisions must NOT go here (just call the tool normally)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nodes": {
                    "type": "array",
                    "description": "Decision nodes in causal order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label":  { "type": "string" },
                            "tool":   { "type": "string" },
                            "args":   { "type": "object" },
                            "pred":   { "type": "string" },
                            "guard":  { "type": "string" }
                        },
                        "required": ["label", "tool", "args"]
                    }
                }
            },
            "required": ["nodes"]
        })
    }

    async fn run(&self, args: &Value) -> anyhow::Result<ToolOutput> {
        let nodes = args
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("missing nodes array"))?;
        let parsed = self.validate(nodes)?;
        let n = self.bank.replace(parsed);
        Ok(ToolOutput {
            text: format!(
                "EDG bank registered: {n} node(s). Guards will be checked against observed tool receipts; eligible actions execute without a model call."
            ),
        })
    }
}

/// Parse an inline ```edg fenced block from assistant text (v2: the
/// model declares the bank in its message instead of spending a tool
/// call round-trip on declare_edg). The block body is the same JSON
/// nodes array as declare_edg's `nodes` parameter. Returns the parsed
/// nodes, or None when no block is present.
pub fn parse_inline(text: &str) -> Option<Vec<EdgNode>> {
    let mut in_block = false;
    let mut body = String::new();
    for line in text.lines() {
        let t = line.trim();
        if !in_block {
            if t == "```edg" {
                in_block = true;
            }
            continue;
        }
        if t == "```" {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    if !in_block {
        return None;
    }
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    let arr = match v {
        Value::Array(a) => a,
        _ => return None,
    };
    let nodes = validate_nodes(&arr).ok()?;
    if nodes.is_empty() {
        return None;
    }
    Some(nodes)
}

/// Standalone node validation (shared by the tool and parse_inline).
fn validate_nodes(nodes: &[Value]) -> anyhow::Result<Vec<EdgNode>> {
    let mut out = Vec::with_capacity(nodes.len());
    for (i, raw) in nodes.iter().enumerate() {
        let label = raw
            .get("label")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("node {i}: missing label"))?
            .to_string();
        let tool = raw
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("node {i}: missing tool"))?
            .to_string();
        let args = raw
            .get("args")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("node {i}: missing args"))?;
        let pred = raw
            .get("pred")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let guard = raw
            .get("guard")
            .and_then(Value::as_str)
            .unwrap_or("last_ok")
            .to_string();
        let node = EdgNode {
            label,
            tool,
            args,
            pred,
            guard,
        };
        if !node.closed() {
            anyhow::bail!(
                "node {} ('{}'): args contain a placeholder — literal closure violated",
                i,
                node.label
            );
        }
        out.push(node);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(label: &str, tool: &str, args: Value, pred: &str, guard: &str) -> Value {
        json!({
            "label": label, "tool": tool, "args": args,
            "pred": pred, "guard": guard
        })
    }

    #[tokio::test]
    async fn literal_closure_rejects_placeholders() {
        let bank = EdgBank::new();
        let tool = DeclareEdgTool { bank: bank.clone() };
        let bad = json!({ "nodes": [
            node("write", "write_file",
                 json!({"path": "/tmp/x.py", "content": "print(1)"}), "", "none"),
            node("run", "bash",
                 json!({"command": "${next_cmd}"}), "write", "last_ok")
        ]});
        let err = tool.run(&bad).await.unwrap_err();
        assert!(err.to_string().contains("literal closure"));

        let good = json!({ "nodes": [
            node("run", "bash",
                 json!({"command": "python3 /tmp/x.py"}), "write", "last_ok")
        ]});
        assert!(tool.run(&good).await.is_ok());
    }

    #[tokio::test]
    async fn guard_requires_receipt() {
        let bank = EdgBank::new();
        // Root-eligible node with 'last_ok' — no receipt yet → None.
        bank.replace(vec![EdgNode {
            label: "run".into(),
            tool: "bash".into(),
            args: json!({"command": "true"}),
            pred: "".into(),
            guard: "last_ok".into(),
        }]);
        assert!(bank.next_eligible(None).is_none());
        // Receipt lands, ok → eligible.
        bank.observe("write_file", true, "wrote 20 bytes");
        assert!(bank.next_eligible(None).is_some());
        // Claim it → single-fire, not eligible again.
        let n = bank.next_eligible(None).unwrap();
        let cert = bank.claim(&n);
        assert_eq!(cert.label, "run");
        assert!(bank.next_eligible(None).is_none());
        assert_eq!(bank.stats().0, 1);
    }

    #[tokio::test]
    async fn chained_nodes_execute_in_order() {
        let bank = EdgBank::new();
        bank.replace(vec![
            EdgNode {
                label: "step1".into(),
                tool: "bash".into(),
                args: json!({"command": "echo a"}),
                pred: "".into(),
                guard: "last_ok".into(),
            },
            EdgNode {
                label: "step2".into(),
                tool: "bash".into(),
                args: json!({"command": "echo b"}),
                pred: "step1".into(),
                guard: "last_ok".into(),
            },
        ]);
        bank.observe("write_file", true, "ok");
        let n1 = bank.next_eligible(None).unwrap();
        assert_eq!(n1.label, "step1");
        bank.claim(&n1);
        bank.observe("bash", true, "a");
        let n2 = bank.next_eligible(None).unwrap();
        assert_eq!(n2.label, "step2");
        bank.claim(&n2);
        assert_eq!(bank.stats().0, 2);
    }

    #[tokio::test]
    async fn failed_pred_drops_dependents() {
        let bank = EdgBank::new();
        bank.replace(vec![EdgNode {
            label: "verify".into(),
            tool: "bash".into(),
            args: json!({"command": "cat /tmp/out.txt"}),
            pred: "gen".into(),
            guard: "last_ok".into(),
        }]);
        bank.record_failure("gen");
        bank.observe("bash", false, "error: exit 1");
        assert!(bank.next_eligible(None).is_none());
    }

    #[test]
    fn file_exists_guard_uses_workspace() {
        let bank = EdgBank::new();
        let dir = std::env::temp_dir();
        bank.replace(vec![EdgNode {
            label: "check".into(),
            tool: "bash".into(),
            args: json!({"command": "true"}),
            pred: "".into(),
            guard: format!("file_exists:{}", dir.display()),
        }]);
        bank.observe("write_file", true, "ok");
        assert!(bank.next_eligible(Some(&dir)).is_some());
    }

    #[test]
    fn unknown_guard_defers() {
        let bank = EdgBank::new();
        bank.replace(vec![EdgNode {
            label: "x".into(),
            tool: "bash".into(),
            args: json!({"command": "true"}),
            pred: "".into(),
            guard: "model_predicts_success".into(),
        }]);
        bank.observe("bash", true, "ok");
        assert!(bank.next_eligible(None).is_none());
    }

    #[test]
    fn inline_parse_extracts_and_validates() {
        let text = "I'll write the module now.\n\
                    ```edg\n\
                    [{\"label\":\"run\",\"tool\":\"bash\",\"args\":{\"command\":\"python3 test_fib.py\"},\"pred\":\"\",\"guard\":\"last_ok\"}]\n\
                    ```\n\
                    Then report.";
        let nodes = parse_inline(text).expect("block should parse");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].tool, "bash");
        // Placeholder args rejected inline too.
        let bad = "```edg\n\
                   [{\"label\":\"r\",\"tool\":\"bash\",\"args\":{\"command\":\"${cmd}\"}}]\n\
                   ```";
        assert!(parse_inline(bad).is_none());
        // No block → None.
        assert!(parse_inline("plain answer, no block").is_none());
    }
}
