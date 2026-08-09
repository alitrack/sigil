//! Harness tests: skill loading and prompt assembly.

use crate::harness::{Harness, PromptState};

#[test]
fn small_skills_are_inlined_into_the_prompt() {
    let dir = std::env::temp_dir().join(format!("sigil-harness-test-{}", std::process::id()));
    let skills = dir.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("tiny.md"), "> A tiny skill\n\njust a few bytes").unwrap();

    let h = Harness::load(Some(&dir)).unwrap();
    let prompt = h.full_system_prompt();

    // Index entry present...
    assert!(prompt.contains("**tiny** — A tiny skill"));
    // ...and the body is inlined.
    assert!(prompt.contains("just a few bytes"));
    assert!(prompt.contains("<skill name=\"tiny\">"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn large_skills_stay_as_index_only() {
    let dir = std::env::temp_dir().join(format!("sigil-harness-test-big-{}", std::process::id()));
    let skills = dir.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    // A skill larger than the 2 KiB inline threshold.
    let big = format!("> Big skill\n\n{}", "x".repeat(3 * 1024));
    std::fs::write(skills.join("big.md"), big).unwrap();

    let h = Harness::load(Some(&dir)).unwrap();
    let prompt = h.full_system_prompt();

    assert!(prompt.contains("**big** — Big skill"));
    // Body is NOT inlined (no <skill> block for it).
    assert!(!prompt.contains("<skill name=\"big\">"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn inline_skills_for_selects_by_relevance() {
    use crate::harness::Skill;
    let skills = vec![
        Skill {
            name: "lua-ffi".into(),
            description: "wrap C libraries with LuaJIT FFI".into(),
            body: "> wrap C\nuse ffi.cdef and ffi.load".into(),
            path: std::path::PathBuf::from("lua-ffi.md"),
            depends_on: vec![],
        },
        Skill {
            name: "git-workflow".into(),
            description: "commit and push workflows".into(),
            body: "> git\ncommit, push, revert".into(),
            path: std::path::PathBuf::from("git-workflow.md"),
            depends_on: vec![],
        },
    ];

    // Prompt about Lua: the Lua skill is inlined, git is not.
    let out = crate::harness::inline_skills_for("how do I use Lua FFI?", &skills, 3);
    assert!(out.contains("<skill name=\"lua-ffi\">"));
    assert!(!out.contains("<skill name=\"git-workflow\">"));

    // Prompt about git: the git skill is inlined, lua is not.
    let out = crate::harness::inline_skills_for("commit my changes", &skills, 3);
    assert!(out.contains("<skill name=\"git-workflow\">"));
    assert!(!out.contains("<skill name=\"lua-ffi\">"));

    // max_inline caps the number of inlined skills.
    let out = crate::harness::inline_skills_for("lua ffi git commit", &skills, 1);
    assert!(out.matches("<skill name=").count() == 1);

    // Empty prompt: nothing inlined.
    let out = crate::harness::inline_skills_for("", &skills, 3);
    assert!(out.is_empty());
}

#[test]
fn depends_on_pulls_in_dependency_skills_cycle_safe() {
    use crate::harness::Skill;
    // math depends on formulas; formulas and notes depend on each other
    // (cycle) — must not loop forever.
    let skills = vec![
        Skill {
            name: "math".into(),
            description: "math rules".into(),
            body: "> math\nmath body".into(),
            path: std::path::PathBuf::from("math.md"),
            depends_on: vec!["formulas".into()],
        },
        Skill {
            name: "formulas".into(),
            description: "formulas".into(),
            body: "> formulas\nformula body".into(),
            path: std::path::PathBuf::from("formulas.md"),
            depends_on: vec!["notes".into()],
        },
        Skill {
            name: "notes".into(),
            description: "notes".into(),
            body: "> notes\nnotes body".into(),
            path: std::path::PathBuf::from("notes.md"),
            depends_on: vec!["formulas".into()],
        },
        Skill {
            name: "unrelated".into(),
            description: "unrelated".into(),
            body: "> unrelated\nother body".into(),
            path: std::path::PathBuf::from("unrelated.md"),
            depends_on: vec![],
        },
    ];

    // Math prompt → math + formulas + notes all inlined (closure),
    // unrelated not.
    let out = crate::harness::inline_skills_for("math problem", &skills, 5);
    assert!(out.contains("<skill name=\"math\">"));
    assert!(out.contains("<skill name=\"formulas\">"));
    assert!(out.contains("<skill name=\"notes\">"));
    assert!(!out.contains("unrelated"));
}

#[test]
fn find_dependency_cycles_reports_cycles() {
    use crate::harness::Skill;
    let skill = |name: &str, deps: Vec<&str>| Skill {
        name: name.into(),
        description: name.into(),
        body: format!("> {name}\nbody"),
        path: std::path::PathBuf::from(format!("{name}.md")),
        depends_on: deps.into_iter().map(String::from).collect(),
    };

    // Acyclic: a → b → c. No cycles.
    let acyclic = vec![
        skill("a", vec!["b"]),
        skill("b", vec!["c"]),
        skill("c", vec![]),
    ];
    assert!(crate::harness::find_dependency_cycles(&acyclic).is_empty());

    // Self-loop: a depends on itself.
    let self_loop = vec![skill("a", vec!["a"])];
    let cycles = crate::harness::find_dependency_cycles(&self_loop);
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0], vec!["a".to_string(), "a".to_string()]);

    // Two-node cycle: a ↔ b. DFS from a finds the a→b→a back edge.
    let two = vec![skill("a", vec!["b"]), skill("b", vec!["a"])];
    let cycles = crate::harness::find_dependency_cycles(&two);
    assert_eq!(cycles.len(), 1);
    assert_eq!(
        cycles[0],
        vec!["a".to_string(), "b".to_string(), "a".to_string()]
    );

    // Unknown dependency names are ignored (no crash, no cycle).
    let unknown = vec![skill("a", vec!["ghost"])];
    assert!(crate::harness::find_dependency_cycles(&unknown).is_empty());
}

#[test]
fn agent_config_inlines_relevant_skills_per_prompt() {
    use crate::agent::AgentConfig;
    use crate::harness::Skill;
    let skills = vec![Skill {
        name: "math-notes".into(),
        description: "math rules".into(),
        body: "> math\nAnswer math questions".into(),
        path: std::path::PathBuf::from("math-notes.md"),
        depends_on: vec![],
    }];

    let cfg = AgentConfig {
        model: "m".into(),
        max_steps: 0,
        prompt_state: std::sync::Arc::new(std::sync::RwLock::new(PromptState {
            system_prompt: "base".into(),
            skills,
        })),
        max_inline_skills: 2,
    };

    // Math prompt → math skill inlined after the base prompt.
    let p = cfg.system_prompt_for("answer this math question");
    assert!(p.starts_with("base"));
    assert!(p.contains("<skill name=\"math-notes\">"));

    // Unrelated prompt → no inlining (no overlap).
    let p = cfg.system_prompt_for("please deploy the service");
    assert_eq!(p, "base");

    // max_inline_skills = 0 → no inlining at all.
    let mut cfg2 = cfg.clone();
    cfg2.max_inline_skills = 0;
    assert_eq!(cfg2.system_prompt_for("answer this math question"), "base");
}

#[test]
fn load_skills_parses_depends_on_from_disk() {
    let dir = std::env::temp_dir().join(format!("sigil-deps-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let skills_dir = dir.join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("report.md"),
        "> 报告技能\n> depends_on: format\n\nbody",
    )
    .unwrap();
    std::fs::write(skills_dir.join("format.md"), "> 格式技能\n\nbody").unwrap();

    let skills = crate::harness::load_skills(&skills_dir);
    let report = skills.iter().find(|s| s.name == "report").unwrap();
    assert_eq!(report.depends_on, vec!["format".to_string()]);
    let format = skills.iter().find(|s| s.name == "format").unwrap();
    assert!(format.depends_on.is_empty());

    // Inlining the report skill pulls format in.
    let out = crate::harness::inline_skills_for("report", &skills, 3);
    assert!(out.contains("<skill name=\"report\">"));
    assert!(out.contains("<skill name=\"format\">"));
    let _ = std::fs::remove_dir_all(&dir);
}
