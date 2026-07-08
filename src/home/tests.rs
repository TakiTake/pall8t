use super::*;

/// A throwaway `~/.pall8t`-shaped root under the system temp dir, removed on
/// drop. Tests here run on Linux (CI/dev container) where `clone_tree` is the
/// recursive-copy fallback, so the full fork/harvest/promote flow is real.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let dir = unique_temp_dir(&format!("test-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        TempRoot(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn base(&self) -> PathBuf {
        base_dir(&self.0)
    }
    fn write_base(&self, rel: &str, content: &str) {
        write_atomic(&self.base().join(rel), content.as_bytes()).unwrap();
    }
    fn read_base(&self, rel: &str) -> Option<String> {
        read_opt(&self.base().join(rel)).map(|b| String::from_utf8(b).unwrap())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Marks a forked run as finished by zeroing its recorded forker pid (0 is
/// never a live process), so `harvest_finished_at` treats it as done. In a
/// real run the forking process — which became `container run` via exec —
/// exiting is what does this; in-process tests share the (live) test pid.
fn finish_run(root: &TempRoot, run: &str) {
    let meta = instances_root(root.path()).join(run).join("meta.toml");
    let text = std::fs::read_to_string(&meta).unwrap();
    let patched = text
        .lines()
        .map(|l| {
            if l.starts_with("forker_pid") {
                "forker_pid = 0".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&meta, patched).unwrap();
}

/// Simulates a run: fork, mutate the instance's `root/` as the agent would,
/// mark it finished, then harvest it.
fn run_and_harvest(root: &TempRoot, run: &str, mutate: impl FnOnce(&Path)) {
    let instance = fork_instance_at(root.path(), run, Path::new("/ws")).unwrap();
    mutate(&instance);
    finish_run(root, run);
    let harvested = harvest_finished_at(root.path(), &[]).unwrap();
    assert!(harvested.contains(&run.to_string()), "run should harvest");
}

fn write(path: &Path, content: &str) {
    write_atomic(path, content.as_bytes()).unwrap();
}

// ---------------------------------------------------------------------------
// Policy / classification
// ---------------------------------------------------------------------------

#[test]
fn glob_matching() {
    assert!(glob_match(
        ".claude/skills/**",
        ".claude/skills/foo/SKILL.md"
    ));
    assert!(glob_match(".claude/skills/**", ".claude/skills")); // ** matches zero segments
    assert!(!glob_match(".claude/skills/**", ".claude/agents/x"));
    assert!(glob_match("**/*.lock", "deep/nested/dir/file.lock"));
    assert!(glob_match("**/*.lock", "top.lock"));
    assert!(!glob_match("**/*.lock", "file.locket"));
    assert!(glob_match(".claude.json", ".claude.json"));
    assert!(!glob_match(".claude.json", ".claude/json"));
    assert!(glob_match("*.txt", "a.txt"));
    assert!(
        !glob_match("*.txt", "sub/a.txt"),
        "* does not cross a slash"
    );
}

#[test]
fn default_classification() {
    let c = |p: &str| classify(p, &[]);
    assert_eq!(c(".claude/.credentials.json").class, Class::Secret);
    assert_eq!(c(".claude.json").class, Class::State);
    assert_eq!(c(".claude/skills/x/SKILL.md").class, Class::Knowledge);
    assert_eq!(c("CLAUDE.md").class, Class::Knowledge);
    assert_eq!(c(".cache/foo").class, Class::Ephemeral);
    assert_eq!(c(".bash_history").class, Class::Ephemeral);
    // Unclassified falls through to staged-knowledge, but is flagged.
    let unk = c(".config/some-new-tool/state");
    assert_eq!(unk.class, Class::Knowledge);
    assert!(!unk.explicit, "unclassified path is not marked explicit");
}

#[test]
fn per_project_memory_is_knowledge_not_ephemeral() {
    // Claude Code's persistent memory lives under the per-project dir; it must
    // beat the broad `.claude/projects/**` ephemeral rule (first match wins),
    // while session transcripts under the same dir stay ephemeral.
    let c = |p: &str| classify(p, &[]);
    assert_eq!(
        c(".claude/projects/some-project/memory/MEMORY.md").class,
        Class::Knowledge,
        "per-project memory must be preserved as knowledge"
    );
    assert_eq!(
        c(".claude/projects/some-project/memory/facts/a-fact.md").class,
        Class::Knowledge
    );
    assert_eq!(
        c(".claude/projects/some-project/transcript.jsonl").class,
        Class::Ephemeral,
        "session transcripts under the project dir stay ephemeral"
    );
}

#[test]
fn override_beats_default_and_first_wins() {
    let overrides = vec![PolicyRule {
        glob: ".claude/skills/**".to_string(),
        class: Some(Class::Ephemeral),
        strategy: None,
    }];
    // Override reclassifies what the default calls knowledge.
    assert_eq!(
        classify(".claude/skills/x", &overrides).class,
        Class::Ephemeral
    );
    // A path the override doesn't touch still hits the defaults.
    assert_eq!(classify(".claude.json", &overrides).class, Class::State);
}

#[test]
fn history_jsonl_is_state_union_by_default() {
    let c = classify(".claude/history.jsonl", &[]);
    assert_eq!(c.class, Class::State);
    assert_eq!(c.strategy, MergeStrategy::Union);
}

#[test]
fn strategy_only_override_defaults_class_to_state() {
    let overrides = vec![PolicyRule {
        glob: ".config/tool/log.jsonl".to_string(),
        class: None,
        strategy: Some(MergeStrategy::Union),
    }];
    let c = classify(".config/tool/log.jsonl", &overrides);
    assert_eq!(
        c.class,
        Class::State,
        "strategy-only rule defaults to state"
    );
    assert_eq!(c.strategy, MergeStrategy::Union);
}

#[test]
fn union_ignored_and_warned_for_secret_ephemeral() {
    // union on a secret/ephemeral is meaningless: classify drops it, and
    // validate_policy warns.
    let overrides = vec![
        PolicyRule {
            glob: ".secret".to_string(),
            class: Some(Class::Secret),
            strategy: Some(MergeStrategy::Union),
        },
        PolicyRule {
            glob: ".nostrat".to_string(),
            class: None,
            strategy: None,
        },
    ];
    assert_eq!(
        classify(".secret", &overrides).strategy,
        MergeStrategy::Inherit,
        "union is ignored for secret"
    );
    let warnings = validate_policy(&overrides);
    assert_eq!(
        warnings.len(),
        2,
        "both the union-on-secret and the empty rule warn"
    );
    assert!(warnings.iter().any(|w| w.contains(".secret")));
    assert!(warnings.iter().any(|w| w.contains(".nostrat")));
}

// ---------------------------------------------------------------------------
// JSON key-path merge (state / FR-1 acceptance #1)
// ---------------------------------------------------------------------------

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[test]
fn json_merge_takes_run_changed_keys_keeps_others() {
    // ancestor -> base advanced key "a"; run changed key "b".
    let ancestor = json(r#"{"a":1,"b":2,"c":3}"#);
    let base = json(r#"{"a":9,"b":2,"c":3}"#); // another run bumped "a"
    let theirs = json(r#"{"a":1,"b":5,"c":3}"#); // this run bumped "b"
    let merged = merge3_json(&ancestor, &base, &theirs);
    assert_eq!(
        merged,
        json(r#"{"a":9,"b":5,"c":3}"#),
        "both changes survive"
    );
}

#[test]
fn json_merge_added_and_deleted_keys() {
    let ancestor = json(r#"{"keep":1,"gone":2}"#);
    let base = json(r#"{"keep":1,"gone":2}"#);
    let theirs = json(r#"{"keep":1,"added":7}"#); // run deleted "gone", added "added"
    let merged = merge3_json(&ancestor, &base, &theirs);
    assert_eq!(merged, json(r#"{"keep":1,"added":7}"#));
}

#[test]
fn json_merge_nested_objects() {
    let ancestor = json(r#"{"o":{"x":1,"y":2}}"#);
    let base = json(r#"{"o":{"x":9,"y":2}}"#); // base changed x
    let theirs = json(r#"{"o":{"x":1,"y":5}}"#); // run changed y
    let merged = merge3_json(&ancestor, &base, &theirs);
    assert_eq!(merged, json(r#"{"o":{"x":9,"y":5}}"#));
}

// ---------------------------------------------------------------------------
// Fork (FR-1)
// ---------------------------------------------------------------------------

#[test]
fn fork_snapshots_base_and_is_removable() {
    let root = TempRoot::new("fork");
    root.write_base(".claude.json", r#"{"v":1}"#);
    root.write_base(".claude/skills/a/SKILL.md", "hello");

    let instance = fork_instance_at(root.path(), "run-1", Path::new("/ws")).unwrap();
    assert_eq!(
        std::fs::read_to_string(instance.join(".claude.json")).unwrap(),
        r#"{"v":1}"#,
        "instance root mirrors the base"
    );
    // Ancestor snapshot exists alongside (not inside) the mounted root.
    let inst_dir = instances_root(root.path()).join("run-1");
    assert!(inst_dir.join("ancestor/.claude.json").exists());
    assert!(inst_dir.join("meta.toml").exists());
    // No leftover partial.
    assert!(!sibling_suffix(&inst_dir, ".partial").exists());
}

#[test]
fn fork_refuses_to_clobber_existing_instance() {
    let root = TempRoot::new("fork-dup");
    root.write_base(".x", "1");
    fork_instance_at(root.path(), "dup", Path::new("/ws")).unwrap();
    let err = fork_instance_at(root.path(), "dup", Path::new("/ws")).unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

// ---------------------------------------------------------------------------
// Harvest (FR-3): secrets, state, ephemeral, knowledge dispositions
// ---------------------------------------------------------------------------

#[test]
fn harvest_writes_back_secret_and_state_stages_knowledge_drops_ephemeral() {
    let root = TempRoot::new("harvest");
    root.write_base(".claude/.credentials.json", r#"{"token":"old"}"#);
    root.write_base(".claude.json", r#"{"projects":{},"n":1}"#);

    run_and_harvest(&root, "run-h", |inst| {
        // Secret refreshed by the run -> latest-wins write-back.
        write(
            &inst.join(".claude/.credentials.json"),
            r#"{"token":"new"}"#,
        );
        // Durable state changed -> key-path merge into base.
        write(&inst.join(".claude.json"), r#"{"projects":{},"n":2}"#);
        // Knowledge -> staged, not written to base.
        write(&inst.join(".claude/skills/s/SKILL.md"), "skill body");
        // Ephemeral -> discarded.
        write(&inst.join(".cache/junk"), "junk");
    });

    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"new"}"#,
        "secret written back latest-wins"
    );
    assert_eq!(
        json(&root.read_base(".claude.json").unwrap()),
        json(r#"{"projects":{},"n":2}"#),
        "state key-path merged"
    );
    // Knowledge is staged in the inbox, NOT in the base (FR-3).
    assert!(
        !root.base().join(".claude/skills/s/SKILL.md").exists(),
        "knowledge must not reach the base at harvest"
    );
    let changesets = list_changesets_at(root.path()).unwrap();
    assert_eq!(changesets.len(), 1);
    assert_eq!(changesets[0].entries, 1, "only the skill is staged");
    // Ephemeral never staged.
    let cs = inbox_root(root.path()).join("run-h");
    assert!(!cs.join("theirs/.cache/junk").exists());
    // Instance disposed after harvest.
    assert!(!instances_root(root.path()).join("run-h").exists());
}

#[test]
fn harvest_skips_live_runs() {
    let root = TempRoot::new("live");
    root.write_base(".x", "1");
    // Forked with the (live) test process as the forker, and not finished.
    fork_instance_at(root.path(), "live-run", Path::new("/ws")).unwrap();
    let harvested = harvest_finished_at(root.path(), &[]).unwrap();
    assert!(harvested.is_empty(), "a live run is not harvested");
    assert!(
        instances_root(root.path()).join("live-run").exists(),
        "the live instance is left in place"
    );
}

#[test]
fn secret_unchanged_by_run_does_not_clobber_base() {
    // FR-10: a run that never touched the credential must not overwrite a
    // token another run refreshed into the base after this fork.
    let root = TempRoot::new("secret-keep");
    root.write_base(".claude/.credentials.json", r#"{"token":"v1"}"#);
    fork_instance_at(root.path(), "r", Path::new("/ws")).unwrap();
    // Base advances (another run refreshed the token) while this run does
    // NOT touch its own copy.
    root.write_base(".claude/.credentials.json", r#"{"token":"v2"}"#);
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &[]).unwrap();
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"v2"}"#,
        "the newer token survives an untouched-secret harvest"
    );
}

// ---------------------------------------------------------------------------
// Promote / drop (FR-4, FR-5)
// ---------------------------------------------------------------------------

#[test]
fn promote_directory_union_adds_new_skill() {
    let root = TempRoot::new("promote-add");
    root.write_base("CLAUDE.md", "base");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/new/SKILL.md"), "new skill");
    });
    let outcome = promote_at(root.path(), "r", &[]).unwrap();
    assert_eq!(outcome.promoted, vec![".claude/skills/new/SKILL.md"]);
    assert!(outcome.conflicts.is_empty());
    assert_eq!(
        root.read_base(".claude/skills/new/SKILL.md").unwrap(),
        "new skill",
        "additive knowledge lands in the base"
    );
    // Changeset consumed.
    assert!(!inbox_root(root.path()).join("r").exists());
}

#[test]
fn promote_selected_path_only() {
    let root = TempRoot::new("promote-sel");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/keep/SKILL.md"), "keep");
        write(&inst.join(".claude/skills/poc/SKILL.md"), "poc");
    });
    let outcome = promote_at(root.path(), "r", &[".claude/skills/keep".to_string()]).unwrap();
    assert_eq!(outcome.promoted, vec![".claude/skills/keep/SKILL.md"]);
    assert!(root.base().join(".claude/skills/keep/SKILL.md").exists());
    assert!(
        !root.base().join(".claude/skills/poc/SKILL.md").exists(),
        "the unselected PoC stays staged, out of the base"
    );
    // The PoC remains in the inbox for a later drop.
    let remaining = list_changesets_at(root.path()).unwrap();
    assert_eq!(remaining[0].entries, 1);
}

#[test]
fn promote_clean_text_three_way_merge() {
    // Base and the run edit well-separated regions (the top vs. the bottom,
    // with unchanged context between) -> clean 3-way merge.
    let root = TempRoot::new("promote-3way");
    root.write_base("CLAUDE.md", "top\na\nb\nc\nbottom\n");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join("CLAUDE.md"), "top\na\nb\nc\nbottom EDITED\n");
    });
    // Base independently changes the first line after the fork.
    root.write_base("CLAUDE.md", "top EDITED\na\nb\nc\nbottom\n");
    let outcome = promote_at(root.path(), "r", &[]).unwrap();
    assert!(outcome.conflicts.is_empty(), "non-overlapping edits merge");
    assert_eq!(
        root.read_base("CLAUDE.md").unwrap(),
        "top EDITED\na\nb\nc\nbottom EDITED\n"
    );
}

#[test]
fn promote_conflict_leaves_base_untouched_and_keeps_staged() {
    // Acceptance #5: overlapping edits conflict at promote; base unchanged.
    let root = TempRoot::new("promote-conflict");
    root.write_base("CLAUDE.md", "shared\n");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join("CLAUDE.md"), "run version\n");
    });
    // Base diverges on the same line.
    root.write_base("CLAUDE.md", "base version\n");
    let outcome = promote_at(root.path(), "r", &[]).unwrap();
    assert_eq!(outcome.conflicts, vec!["CLAUDE.md"]);
    assert!(outcome.promoted.is_empty());
    assert_eq!(
        root.read_base("CLAUDE.md").unwrap(),
        "base version\n",
        "conflict must not poison the base"
    );
    // Still staged so a resolved re-run can complete.
    assert!(inbox_root(root.path()).join("r").exists());
}

#[test]
fn drop_removes_changeset() {
    let root = TempRoot::new("drop");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    drop_changeset_at(root.path(), "r", &[]).unwrap();
    assert!(list_changesets_at(root.path()).unwrap().is_empty());
    assert!(!root.base().join(".claude/skills/x/SKILL.md").exists());
}

#[test]
fn drop_selected_path_keeps_the_rest() {
    let root = TempRoot::new("drop-sel");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/a/SKILL.md"), "a");
        write(&inst.join(".claude/skills/b/SKILL.md"), "b");
    });
    drop_changeset_at(root.path(), "r", &[".claude/skills/a".to_string()]).unwrap();
    let remaining = list_changesets_at(root.path()).unwrap();
    assert_eq!(remaining[0].entries, 1, "only b remains");
}

#[test]
fn unknown_promote_path_errors() {
    let root = TempRoot::new("promote-typo");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/real/SKILL.md"), "x");
    });
    let err = promote_at(root.path(), "r", &[".claude/skills/typo".to_string()]).unwrap_err();
    assert!(err.to_string().contains("no staged path"));
}

// ---------------------------------------------------------------------------
// Two parallel runs (acceptance #1, #2)
// ---------------------------------------------------------------------------

#[test]
fn two_parallel_runs_merge_state_and_stage_independently() {
    let root = TempRoot::new("parallel");
    root.write_base(".claude.json", r#"{"shared":0,"a":0,"b":0}"#);

    // Both fork from the same base.
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa")).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb")).unwrap();

    // A bumps key "a" and adds skill X; B bumps key "b" and adds skill Y.
    write(&inst_a.join(".claude.json"), r#"{"shared":0,"a":1,"b":0}"#);
    write(&inst_a.join(".claude/skills/x/SKILL.md"), "X");
    write(&inst_b.join(".claude.json"), r#"{"shared":0,"a":0,"b":1}"#);
    write(&inst_b.join(".claude/skills/y/SKILL.md"), "Y");

    // Both runs end; harvest both (serialized under the base lock).
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[]).unwrap();

    // Durable keys from both runs merged automatically, no corruption.
    assert_eq!(
        json(&root.read_base(".claude.json").unwrap()),
        json(r#"{"shared":0,"a":1,"b":1}"#)
    );
    // Both changesets sit independently in the inbox.
    let changesets = list_changesets_at(root.path()).unwrap();
    assert_eq!(changesets.len(), 2);

    // Promoting both lands both skills without conflict.
    promote_at(root.path(), "run-a", &[]).unwrap();
    promote_at(root.path(), "run-b", &[]).unwrap();
    assert!(root.base().join(".claude/skills/x/SKILL.md").exists());
    assert!(root.base().join(".claude/skills/y/SKILL.md").exists());
}

#[test]
fn parallel_history_jsonl_appends_union_at_harvest_no_conflict() {
    // Acceptance: two parallel runs each append a different line to the
    // append-only history; after both harvests the base contains both lines,
    // nothing conflicts, and nothing is staged (it's state, not knowledge).
    let root = TempRoot::new("history-union");
    root.write_base(".claude/history.jsonl", "{\"p\":\"x\"}\n");
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa")).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb")).unwrap();
    write(
        &inst_a.join(".claude/history.jsonl"),
        "{\"p\":\"x\"}\n{\"p\":\"a\"}\n",
    );
    write(
        &inst_b.join(".claude/history.jsonl"),
        "{\"p\":\"x\"}\n{\"p\":\"b\"}\n",
    );
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[]).unwrap();

    let merged = root.read_base(".claude/history.jsonl").unwrap();
    assert!(merged.contains("\"p\":\"x\""), "base line kept");
    assert!(merged.contains("\"p\":\"a\""), "run-a's append kept");
    assert!(merged.contains("\"p\":\"b\""), "run-b's append kept");
    assert!(
        list_changesets_at(root.path()).unwrap().is_empty(),
        "history is state, never staged"
    );
}

#[test]
fn union_non_utf8_run_keeps_base_not_overwrites() {
    // A crash-truncated (non-UTF-8) history.jsonl must not overwrite the base
    // and drop the lines other runs accumulated — the base is left unchanged.
    let root = TempRoot::new("history-badutf8");
    let base_content = "{\"p\":\"acc-from-other-runs\"}\n";
    root.write_base(".claude/history.jsonl", base_content);
    let inst = fork_instance_at(root.path(), "r", Path::new("/ws")).unwrap();
    // Invalid UTF-8 bytes as the run's history.
    write_atomic(
        &inst.join(".claude/history.jsonl"),
        &[0xff, 0xfe, 0x00, 0xff],
    )
    .unwrap();
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &[]).unwrap();
    assert_eq!(
        root.read_base(".claude/history.jsonl").unwrap(),
        base_content,
        "base kept intact rather than overwritten by the corrupt run file"
    );
    // The instance is still disposed (union non-UTF-8 warns, doesn't error).
    assert!(!instances_root(root.path()).join("r").exists());
}

#[test]
fn knowledge_union_strategy_merges_without_conflict_at_promote() {
    // A knowledge-class file marked strategy=union merges cleanly at promote
    // where a plain 3-way (overlapping middle-line edits) would conflict.
    let overrides = vec![PolicyRule {
        glob: "notes.log".to_string(),
        class: Some(Class::Knowledge),
        strategy: Some(MergeStrategy::Union),
    }];
    let root = TempRoot::new("knowledge-union");
    root.write_base("notes.log", "top\nshared\nbottom\n");
    // Run edits the middle line; harvest with the override stages it as
    // knowledge+union.
    let inst = fork_instance_at(root.path(), "r", Path::new("/ws")).unwrap();
    write(&inst.join("notes.log"), "top\nrun\nbottom\n");
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &overrides).unwrap();
    // Base diverges on the same middle line -> plain 3-way would conflict.
    root.write_base("notes.log", "top\nbase\nbottom\n");

    let outcome = promote_at(root.path(), "r", &[]).unwrap();
    assert!(
        outcome.conflicts.is_empty(),
        "union strategy must not conflict"
    );
    let merged = root.read_base("notes.log").unwrap();
    assert!(merged.contains("base"), "base's edit kept");
    assert!(merged.contains("run"), "run's edit kept");
}

#[test]
fn two_parallel_runs_both_create_state_file() {
    // The harder acceptance #1 case: `.claude.json` is absent at fork, so
    // each run's ancestor snapshot has no copy. A whole-file overwrite would
    // let the second harvest erase the first run's keys; the empty-object
    // merge base keeps both.
    let root = TempRoot::new("parallel-create");
    // No .claude.json in the base.
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa")).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb")).unwrap();
    write(&inst_a.join(".claude.json"), r#"{"runA":1}"#);
    write(&inst_b.join(".claude.json"), r#"{"runB":2}"#);
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[]).unwrap();
    assert_eq!(
        json(&root.read_base(".claude.json").unwrap()),
        json(r#"{"runA":1,"runB":2}"#),
        "both runs' keys survive even with no fork-point ancestor"
    );
}

#[test]
fn state_merge_absent_ancestor_is_key_level_not_overwrite() {
    // Base already has a concurrent run's key; this run created its own key
    // with no fork-point ancestor -> key-level merge, not overwrite.
    let merged = state_merge_bytes(Some(br#"{"other":1}"#), None, br#"{"mine":2}"#).unwrap();
    assert_eq!(
        json(&String::from_utf8(merged).unwrap()),
        json(r#"{"other":1,"mine":2}"#)
    );
}

// ---------------------------------------------------------------------------
// Liveness, disposal, idempotency (FR-6 / FR-8 / acceptance #7)
// ---------------------------------------------------------------------------

#[test]
fn forker_pid_liveness() {
    // The test process is alive; pid 0 never is.
    assert!(is_forker_alive(std::process::id()));
    assert!(!is_forker_alive(0));
}

#[test]
fn harvest_disposes_instance_leaving_no_tombstone() {
    let root = TempRoot::new("dispose");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    let inst = instances_root(root.path()).join("r");
    assert!(!inst.exists(), "instance gone");
    assert!(
        !sibling_suffix(&inst, ".discard").exists(),
        "no tombstone left behind"
    );
}

#[test]
fn second_harvest_of_same_instance_is_a_noop() {
    // Models the concurrent case: the instance was already drained+disposed.
    // A second harvest_instance on the vanished path reports "already gone"
    // rather than erroring or double-counting.
    let root = TempRoot::new("noop");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    let gone = harvest_instance(root.path(), &instances_root(root.path()).join("r"), &[]).unwrap();
    assert!(!gone, "harvesting a disposed instance is a no-op");
}

#[test]
fn fork_refuses_over_unpromoted_changeset() {
    // FR-9: a harvested-but-un-promoted changeset must not be clobbered by a
    // later run that reuses the same name (recycled pid, same cwd).
    let root = TempRoot::new("reuse");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    // Changeset "r" now sits un-promoted in the inbox; re-forking "r" fails.
    let err = fork_instance_at(root.path(), "r", Path::new("/ws")).unwrap_err();
    assert!(err.to_string().contains("un-promoted changeset"));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// merge = harvest + show + promote-all (FR-4/FR-11 convenience)
// ---------------------------------------------------------------------------

/// Forks + mutates + finishes a run WITHOUT harvesting (merge does the harvest).
fn stage_run(root: &TempRoot, run: &str, mutate: impl FnOnce(&Path)) {
    let inst = fork_instance_at(root.path(), run, Path::new("/ws")).unwrap();
    mutate(&inst);
    finish_run(root, run);
}

#[test]
fn merge_promotes_all_pending() {
    let root = TempRoot::new("merge-all");
    stage_run(&root, "run-a", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "X");
    });
    stage_run(&root, "run-b", |inst| {
        write(&inst.join(".claude/skills/y/SKILL.md"), "Y");
    });
    let report = merge_at(root.path(), None, &[]).unwrap();
    assert_eq!(report.steps.len(), 2, "both pending runs processed");
    assert!(report.steps.iter().all(|s| s.conflicts.is_empty()));
    assert_eq!(report.steps[0].run, "run-a", "oldest fork first");
    // Both skills landed and the inbox is drained.
    assert!(root.base().join(".claude/skills/x/SKILL.md").exists());
    assert!(root.base().join(".claude/skills/y/SKILL.md").exists());
    assert!(list_changesets_at(root.path()).unwrap().is_empty());
}

#[test]
fn merge_specific_run_only() {
    let root = TempRoot::new("merge-one");
    stage_run(&root, "run-a", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "X");
    });
    stage_run(&root, "run-b", |inst| {
        write(&inst.join(".claude/skills/y/SKILL.md"), "Y");
    });
    let report = merge_at(root.path(), Some("run-a"), &[]).unwrap();
    assert_eq!(report.steps.len(), 1);
    assert_eq!(report.steps[0].run, "run-a");
    assert!(root.base().join(".claude/skills/x/SKILL.md").exists());
    assert!(
        !root.base().join(".claude/skills/y/SKILL.md").exists(),
        "run-b untouched — still forked, not merged"
    );
}

#[test]
fn merge_stops_at_conflict_leaving_remainder() {
    let root = TempRoot::new("merge-conflict");
    root.write_base("CLAUDE.md", "shared\n");
    // run-a (older) edits CLAUDE.md; run-b (newer) adds a skill.
    stage_run(&root, "run-a", |inst| {
        write(&inst.join("CLAUDE.md"), "run-a version\n");
    });
    stage_run(&root, "run-b", |inst| {
        write(&inst.join(".claude/skills/y/SKILL.md"), "Y");
    });
    // Base diverges on the same line after the forks, so run-a's promote
    // conflicts.
    root.write_base("CLAUDE.md", "base version\n");

    let report = merge_at(root.path(), None, &[]).unwrap();
    assert_eq!(
        report.steps.len(),
        1,
        "stopped at the first (conflicting) run"
    );
    assert_eq!(report.steps[0].run, "run-a");
    assert_eq!(report.steps[0].conflicts, vec!["CLAUDE.md"]);
    assert_eq!(
        root.read_base("CLAUDE.md").unwrap(),
        "base version\n",
        "conflict does not poison the base"
    );
    // Both the conflicting changeset and the untouched later one remain.
    let pending: Vec<String> = list_changesets_at(root.path())
        .unwrap()
        .into_iter()
        .map(|c| c.run)
        .collect();
    assert!(pending.contains(&"run-a".to_string()));
    assert!(
        pending.contains(&"run-b".to_string()),
        "the later changeset is left staged, not merged"
    );
    assert!(!root.base().join(".claude/skills/y/SKILL.md").exists());
}

#[test]
fn merge_empty_inbox_is_noop() {
    let root = TempRoot::new("merge-empty");
    let report = merge_at(root.path(), None, &[]).unwrap();
    assert!(report.steps.is_empty(), "nothing pending -> no steps");
    assert!(report.harvested.is_empty());
}

#[test]
fn merge_specific_live_run_errors() {
    let root = TempRoot::new("merge-live");
    // Forked with the live test pid, not finished.
    fork_instance_at(root.path(), "run-live", Path::new("/ws")).unwrap();
    let err = merge_at(root.path(), Some("run-live"), &[]).unwrap_err();
    assert!(err.to_string().contains("still running"));
}

#[test]
fn merge_unknown_run_errors() {
    // A typo'd run (no instance, no changeset) errors rather than silently
    // succeeding — parity with promote/drop/show.
    let root = TempRoot::new("merge-unknown");
    let err = merge_at(root.path(), Some("run-typo"), &[]).unwrap_err();
    assert!(err.to_string().contains("no run run-typo"));
}

#[test]
fn merge_secrets_only_run_is_a_clean_noop() {
    // A run that changed only a secret harvests (writing the base) but stages
    // no changeset; `merge <run>` succeeds with no promote steps.
    let root = TempRoot::new("merge-secrets");
    root.write_base(".claude/.credentials.json", r#"{"token":"old"}"#);
    stage_run(&root, "run-s", |inst| {
        write(
            &inst.join(".claude/.credentials.json"),
            r#"{"token":"new"}"#,
        );
    });
    let report = merge_at(root.path(), Some("run-s"), &[]).unwrap();
    assert!(report.steps.is_empty(), "no knowledge to promote");
    assert_eq!(report.harvested, vec!["run-s"], "but the run was harvested");
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"new"}"#,
        "the secret was written back"
    );
}

#[test]
fn epoch_formatting() {
    assert_eq!(fmt_epoch(0), "1970-01-01 00:00:00Z");
    assert_eq!(fmt_epoch(1_700_000_000), "2023-11-14 22:13:20Z");
}

#[test]
fn atomic_write_replaces_and_creates_dirs() {
    let root = TempRoot::new("atomic");
    let p = root.path().join("deep/nested/file");
    write_atomic(&p, b"one").unwrap();
    write_atomic(&p, b"two").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"two");
}
