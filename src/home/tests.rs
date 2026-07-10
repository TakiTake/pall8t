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
    let instance = fork_instance_at(root.path(), run, Path::new("/ws"), &[]).unwrap();
    mutate(&instance);
    finish_run(root, run);
    let harvested = harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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

    let instance = fork_instance_at(root.path(), "run-1", Path::new("/ws"), &[]).unwrap();
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
    fork_instance_at(root.path(), "dup", Path::new("/ws"), &[]).unwrap();
    let err = fork_instance_at(root.path(), "dup", Path::new("/ws"), &[]).unwrap_err();
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
    fork_instance_at(root.path(), "live-run", Path::new("/ws"), &[]).unwrap();
    let harvested = harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    // Base advances (another run refreshed the token) while this run does
    // NOT touch its own copy.
    root.write_base(".claude/.credentials.json", r#"{"token":"v2"}"#);
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let outcome = promote_at(
        root.path(),
        "r",
        &[".claude/skills/keep".to_string()],
        &[],
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();
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
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let err = promote_at(
        root.path(),
        "r",
        &[".claude/skills/typo".to_string()],
        &[],
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap_err();
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
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa"), &[]).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb"), &[]).unwrap();

    // A bumps key "a" and adds skill X; B bumps key "b" and adds skill Y.
    write(&inst_a.join(".claude.json"), r#"{"shared":0,"a":1,"b":0}"#);
    write(&inst_a.join(".claude/skills/x/SKILL.md"), "X");
    write(&inst_b.join(".claude.json"), r#"{"shared":0,"a":0,"b":1}"#);
    write(&inst_b.join(".claude/skills/y/SKILL.md"), "Y");

    // Both runs end; harvest both (serialized under the base lock).
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();

    // Durable keys from both runs merged automatically, no corruption.
    assert_eq!(
        json(&root.read_base(".claude.json").unwrap()),
        json(r#"{"shared":0,"a":1,"b":1}"#)
    );
    // Both changesets sit independently in the inbox.
    let changesets = list_changesets_at(root.path()).unwrap();
    assert_eq!(changesets.len(), 2);

    // Promoting both lands both skills without conflict.
    promote_at(root.path(), "run-a", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
    promote_at(root.path(), "run-b", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa"), &[]).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb"), &[]).unwrap();
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
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();

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
    let inst = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    // Invalid UTF-8 bytes as the run's history.
    write_atomic(
        &inst.join(".claude/history.jsonl"),
        &[0xff, 0xfe, 0x00, 0xff],
    )
    .unwrap();
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    // Forked WITH the override in effect (fork-time policy is authoritative
    // at harvest — see `InstanceMeta::policy`); run edits the middle line,
    // harvest stages it as knowledge+union.
    let inst = fork_instance_at(root.path(), "r", Path::new("/ws"), &overrides).unwrap();
    write(&inst.join("notes.log"), "top\nrun\nbottom\n");
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &overrides, DEFAULT_REVISIONS_KEEP).unwrap();
    // Base diverges on the same middle line -> plain 3-way would conflict.
    root.write_base("notes.log", "top\nbase\nbottom\n");

    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa"), &[]).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb"), &[]).unwrap();
    write(&inst_a.join(".claude.json"), r#"{"runA":1}"#);
    write(&inst_b.join(".claude.json"), r#"{"runB":2}"#);
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let gone = harvest_instance(
        root.path(),
        &instances_root(root.path()).join("r"),
        &[],
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();
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
    let err = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap_err();
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
    let inst = fork_instance_at(root.path(), run, Path::new("/ws"), &[]).unwrap();
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
    let report = merge_at(root.path(), None, &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let report = merge_at(root.path(), Some("run-a"), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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

    let report = merge_at(root.path(), None, &[], DEFAULT_REVISIONS_KEEP).unwrap();
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
    let report = merge_at(root.path(), None, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert!(report.steps.is_empty(), "nothing pending -> no steps");
    assert!(report.harvested.is_empty());
}

#[test]
fn merge_specific_live_run_errors() {
    let root = TempRoot::new("merge-live");
    // Forked with the live test pid, not finished.
    fork_instance_at(root.path(), "run-live", Path::new("/ws"), &[]).unwrap();
    let err = merge_at(root.path(), Some("run-live"), &[], DEFAULT_REVISIONS_KEEP).unwrap_err();
    assert!(err.to_string().contains("still running"));
}

#[test]
fn merge_unknown_run_errors() {
    // A typo'd run (no instance, no changeset) errors rather than silently
    // succeeding — parity with promote/drop/show.
    let root = TempRoot::new("merge-unknown");
    let err = merge_at(root.path(), Some("run-typo"), &[], DEFAULT_REVISIONS_KEEP).unwrap_err();
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
    let report = merge_at(root.path(), Some("run-s"), &[], DEFAULT_REVISIONS_KEEP).unwrap();
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

// ---------------------------------------------------------------------------
// Test helpers for Phase 2 (backdating timestamps/mtimes)
// ---------------------------------------------------------------------------

/// Rewrites a published instance's `meta.toml` `created` field so `ls` sees
/// it as older, mirroring `finish_run`'s line-patch approach.
fn backdate_instance_created(root: &TempRoot, run: &str, secs_ago: u64) {
    let meta_path = instances_root(root.path()).join(run).join("meta.toml");
    let text = std::fs::read_to_string(&meta_path).unwrap();
    let target = now_secs().saturating_sub(secs_ago);
    let patched = text
        .lines()
        .map(|l| {
            if l.starts_with("created") {
                format!("created = {target}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&meta_path, patched).unwrap();
}

/// Rewrites a changeset's `manifest.toml` `created` field so `gc`'s TTL
/// check sees it as older.
fn backdate_changeset(root: &TempRoot, run: &str, days_ago: u64) {
    let path = inbox_root(root.path()).join(run).join("manifest.toml");
    let mut m: Manifest = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    m.created = now_secs().saturating_sub(days_ago * 86_400 + 3_600);
    std::fs::write(&path, toml::to_string_pretty(&m).unwrap()).unwrap();
}

/// Sets a path's mtime `secs_ago` seconds in the past via `touch -d @<epoch>`
/// (GNU coreutils, available in this Linux test environment) — avoids
/// pulling in a `filetime` dependency just for one test's staleness check.
fn set_mtime_seconds_ago(path: &Path, secs_ago: u64) {
    let epoch = now_secs().saturating_sub(secs_ago);
    let status = std::process::Command::new("touch")
        .arg("-d")
        .arg(format!("@{epoch}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "touch -d failed");
}

// ---------------------------------------------------------------------------
// Revisions (FR-7): log / diff / rollback
// ---------------------------------------------------------------------------

#[test]
fn harvest_records_a_revision_only_when_it_writes_the_base() {
    let root = TempRoot::new("rev-harvest");
    root.write_base(".claude/.credentials.json", r#"{"token":"old"}"#);

    // Knowledge-only run: base never mutated at harvest -> no revision.
    run_and_harvest(&root, "know-only", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    assert!(
        list_revisions_at(root.path()).unwrap().is_empty(),
        "staging knowledge alone must not record a revision"
    );

    // Secret-changing run: base mutated -> exactly one revision.
    run_and_harvest(&root, "sec", |inst| {
        write(
            &inst.join(".claude/.credentials.json"),
            r#"{"token":"new"}"#,
        );
    });
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    assert_eq!(revs[0].op, RevisionOp::Harvest);
    assert_eq!(revs[0].runs, vec!["sec".to_string()]);
    assert_eq!(revs[0].paths, 1);
}

#[test]
fn revision_snapshot_is_the_pre_mutation_base() {
    let root = TempRoot::new("rev-snapshot");
    root.write_base(".claude/.credentials.json", r#"{"token":"v1"}"#);
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/.credentials.json"), r#"{"token":"v2"}"#);
    });
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    let snap = revisions_root(root.path())
        .join(seq_name(revs[0].seq))
        .join("snapshot/.claude/.credentials.json");
    assert_eq!(
        std::fs::read_to_string(snap).unwrap(),
        r#"{"token":"v1"}"#,
        "the snapshot captures the base as it was BEFORE the mutation"
    );
}

#[test]
fn promote_records_a_revision_only_for_landed_paths() {
    let root = TempRoot::new("rev-promote");
    root.write_base("CLAUDE.md", "shared\n");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/a/SKILL.md"), "a");
        write(&inst.join("CLAUDE.md"), "run version\n");
    });
    // Base diverges so CLAUDE.md conflicts; the skill still lands cleanly.
    root.write_base("CLAUDE.md", "base version\n");
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(outcome.promoted, vec![".claude/skills/a/SKILL.md"]);
    assert_eq!(outcome.conflicts, vec!["CLAUDE.md"]);

    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1, "one revision for the landed path only");
    assert_eq!(revs[0].op, RevisionOp::Promote);
    assert_eq!(revs[0].paths, 1);
}

#[test]
fn promote_landing_nothing_records_no_revision() {
    let root = TempRoot::new("rev-promote-none");
    root.write_base("CLAUDE.md", "shared\n");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join("CLAUDE.md"), "run version\n");
    });
    root.write_base("CLAUDE.md", "base version\n");
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert!(outcome.promoted.is_empty());
    assert!(
        list_revisions_at(root.path()).unwrap().is_empty(),
        "a promote that lands nothing must not burn a revision"
    );
}

#[test]
fn revisions_pruned_beyond_keep() {
    let root = TempRoot::new("rev-prune");
    root.write_base(".claude/.credentials.json", r#"{"token":"0"}"#);
    for i in 1..=5u32 {
        let run = format!("r{i}");
        let instance = fork_instance_at(root.path(), &run, Path::new("/ws"), &[]).unwrap();
        write(
            &instance.join(".claude/.credentials.json"),
            &format!(r#"{{"token":"{i}"}}"#),
        );
        finish_run(&root, &run);
        harvest_finished_at(root.path(), &[], 2).unwrap();
    }
    let dirs = list_revision_dirs(root.path()).unwrap();
    assert_eq!(dirs.len(), 2, "pruned down to revisions_keep=2 each time");
    let seqs: Vec<u64> = dirs.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, vec![4, 5], "the two newest survive");
}

#[test]
fn diff_redacts_secret_content_but_shows_the_path() {
    let root = TempRoot::new("rev-diff-secret");
    root.write_base(
        ".claude/.credentials.json",
        r#"{"token":"old-secret-value"}"#,
    );
    run_and_harvest(&root, "r", |inst| {
        write(
            &inst.join(".claude/.credentials.json"),
            r#"{"token":"new-secret-value"}"#,
        );
    });
    let revs = list_revisions_at(root.path()).unwrap();
    let out = diff_at(root.path(), revs[0].seq, &[]).unwrap();
    assert!(out.contains(".claude/.credentials.json"));
    assert!(out.contains("secret — content not shown"));
    assert!(!out.contains("old-secret-value"));
    assert!(!out.contains("new-secret-value"));
}

#[test]
fn diff_newest_revision_compares_against_current_base() {
    let root = TempRoot::new("rev-diff-newest");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/skills/a/SKILL.md"), "a");
    });
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(outcome.promoted.len(), 1);
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    let out = diff_at(root.path(), revs[0].seq, &[]).unwrap();
    assert!(out.contains(".claude/skills/a/SKILL.md"));
    assert!(out.contains("added"));
}

#[test]
fn diff_unknown_revision_errors() {
    let root = TempRoot::new("rev-diff-unknown");
    let err = diff_at(root.path(), 999, &[]).unwrap_err();
    assert!(err.to_string().contains("no revision"));
}

#[test]
fn rollback_restores_base_and_is_itself_a_revision() {
    let root = TempRoot::new("rollback");
    root.write_base(".claude/.credentials.json", r#"{"token":"v1"}"#);
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/.credentials.json"), r#"{"token":"v2"}"#);
    });
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"v2"}"#
    );
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    let seq = revs[0].seq;

    rollback_at(root.path(), seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"v1"}"#,
        "base restored to the pre-mutation snapshot"
    );

    let revs_after = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs_after.len(), 2, "the rollback itself is a new revision");
    assert_eq!(revs_after[0].op, RevisionOp::Rollback, "newest first");
    assert_eq!(revs_after[0].paths, 1);

    // The rollback is itself undoable: rolling back to its own pre-mutation
    // snapshot (the state right after the original harvest) restores v2.
    rollback_at(root.path(), revs_after[0].seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"v2"}"#,
        "rolling back the rollback undoes it"
    );
}

#[test]
fn rollback_inherits_earlier_revisions_policy_to_protect_reintroduced_secret_content() {
    // Revision 1 (harvest) recorded policy A, which declares a path secret.
    // Rolling back to it later, invoked with a DIFFERENT (empty) policy --
    // as if from a different project, or much later once the rule is no
    // longer loaded -- must still protect that content in the ROLLBACK's
    // OWN new revision's diff, not just the original harvest revision's.
    // Without accumulating history's recorded policies into the rollback's
    // own, this would leak: rollback never classifies anything itself, and
    // neither the rollback-time nor the diff-time cwd policy would know the
    // rule.
    let policy_a = vec![PolicyRule {
        glob: ".config/mytool/secret".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];
    let root = TempRoot::new("rollback-inherits-policy");
    root.write_base(".config/mytool/secret", "old-value");
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &policy_a).unwrap();
    write(&instance.join(".config/mytool/secret"), "new-value");
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &policy_a, DEFAULT_REVISIONS_KEEP).unwrap();

    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    let harvest_seq = revs[0].seq;
    assert_eq!(
        root.read_base(".config/mytool/secret").unwrap(),
        "new-value"
    );

    // Rolled back with an EMPTY current policy -- restores "old-value".
    rollback_at(root.path(), harvest_seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(
        root.read_base(".config/mytool/secret").unwrap(),
        "old-value"
    );

    let revs_after = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs_after.len(), 2);
    let rollback_seq = revs_after[0].seq; // newest first

    // Diffed with an EMPTY current policy too -- only the accumulated
    // history (policy_a, still recorded on the harvest revision) protects it.
    let out = diff_at(root.path(), rollback_seq, &[]).unwrap();
    assert!(out.contains(".config/mytool/secret"));
    assert!(out.contains("secret — content not shown"));
    assert!(!out.contains("old-value"));
    assert!(!out.contains("new-value"));
}

#[test]
fn rollback_accumulated_policy_filters_to_secret_rules_avoiding_masking() {
    // Revision 1 (older, a promote) recorded a BROAD non-secret rule
    // (`.config/mytool/**` -> knowledge). Revision 2 (newer, a harvest)
    // recorded a SPECIFIC secret rule for the token path. `classify` is
    // first-match-wins, so naively concatenating raw policy lists
    // oldest-first would let revision 1's broad rule mask revision 2's
    // secret rule when the ROLLBACK's own (later) revision is diffed --
    // `accumulated_secret_policy` must filter both sources to
    // secret-classifying rules only so this can't happen.
    let broad_knowledge_rule = vec![PolicyRule {
        glob: ".config/mytool/**".to_string(),
        class: Some(Class::Knowledge),
        strategy: None,
    }];
    let secret_rule = vec![PolicyRule {
        glob: ".config/mytool/token".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];

    let root = TempRoot::new("rollback-masking-hazard");
    root.write_base(".config/mytool/token", "old-value");

    // Revision 1 (older): promote a knowledge path under the broad rule.
    let inst_a =
        fork_instance_at(root.path(), "a", Path::new("/wa"), &broad_knowledge_rule).unwrap();
    write(&inst_a.join(".config/mytool/notes"), "some notes");
    finish_run(&root, "a");
    harvest_finished_at(root.path(), &broad_knowledge_rule, DEFAULT_REVISIONS_KEEP).unwrap();
    let outcome = promote_at(
        root.path(),
        "a",
        &[],
        &broad_knowledge_rule,
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();
    assert_eq!(outcome.promoted, vec![".config/mytool/notes".to_string()]);

    // Revision 2 (newer): harvest changes the token under the secret rule.
    let inst_b = fork_instance_at(root.path(), "b", Path::new("/wb"), &secret_rule).unwrap();
    write(&inst_b.join(".config/mytool/token"), "new-value");
    finish_run(&root, "b");
    harvest_finished_at(root.path(), &secret_rule, DEFAULT_REVISIONS_KEEP).unwrap();

    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(
        revs.len(),
        2,
        "revision 1 (promote) and revision 2 (harvest)"
    );
    let harvest_seq = revs[0].seq; // newest first -> the harvest is revs[0]
    assert_eq!(root.read_base(".config/mytool/token").unwrap(), "new-value");

    // Rolled back to the harvest revision's pre-mutation snapshot, invoked
    // with an EMPTY current policy.
    rollback_at(root.path(), harvest_seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(root.read_base(".config/mytool/token").unwrap(), "old-value");

    let revs_after = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs_after.len(), 3);
    let rollback_seq = revs_after[0].seq; // newest first

    let out = diff_at(root.path(), rollback_seq, &[]).unwrap();
    assert!(out.contains(".config/mytool/token"));
    assert!(
        out.contains("secret — content not shown"),
        "the secret rule must not be masked by the broader, older knowledge rule"
    );
    assert!(!out.contains("old-value"));
    assert!(!out.contains("new-value"));
}

#[test]
fn rollback_unknown_revision_errors() {
    let root = TempRoot::new("rollback-unknown");
    let err = rollback_at(root.path(), 999, &[], DEFAULT_REVISIONS_KEEP).unwrap_err();
    assert!(err.to_string().contains("no revision"));
}

#[test]
fn rollback_to_already_current_state_records_nothing_new() {
    let root = TempRoot::new("rollback-twice");
    root.write_base(".claude/.credentials.json", r#"{"token":"v1"}"#);
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude/.credentials.json"), r#"{"token":"v2"}"#);
    });
    let seq = list_revisions_at(root.path()).unwrap()[0].seq;
    rollback_at(root.path(), seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    let count_after_first = list_revisions_at(root.path()).unwrap().len();
    // The base is already at revision `seq`'s snapshot content; rolling back
    // to it again changes nothing.
    rollback_at(root.path(), seq, &[], DEFAULT_REVISIONS_KEEP).unwrap();
    let count_after_second = list_revisions_at(root.path()).unwrap().len();
    assert_eq!(
        count_after_first, count_after_second,
        "rolling back to a state the base is already in records nothing new"
    );
}

// ---------------------------------------------------------------------------
// Instance & inbox lifecycle (FR-9): ls / rm / gc
// ---------------------------------------------------------------------------

#[test]
fn ls_reports_running_and_finished_instances() {
    let root = TempRoot::new("ls");
    fork_instance_at(root.path(), "live", Path::new("/ws"), &[]).unwrap(); // live test pid
    fork_instance_at(root.path(), "done", Path::new("/ws"), &[]).unwrap();
    finish_run(&root, "done");

    let mut instances = list_instances_at(root.path()).unwrap();
    instances.sort_by(|a, b| a.run.cmp(&b.run));
    assert_eq!(instances.len(), 2);
    let live = instances.iter().find(|i| i.run == "live").unwrap();
    assert_eq!(live.status, InstanceStatus::Running);
    assert!(
        !live.suspicious,
        "a freshly forked live run is not suspicious"
    );
    let done = instances.iter().find(|i| i.run == "done").unwrap();
    assert_eq!(done.status, InstanceStatus::Finished);
}

#[test]
fn ls_flags_implausibly_old_running_instance_as_suspicious() {
    let root = TempRoot::new("ls-suspicious");
    fork_instance_at(root.path(), "old", Path::new("/ws"), &[]).unwrap();
    backdate_instance_created(&root, "old", SUSPICIOUS_RUNNING_SECS + 3_600);
    let instances = list_instances_at(root.path()).unwrap();
    let old = instances.iter().find(|i| i.run == "old").unwrap();
    assert_eq!(
        old.status,
        InstanceStatus::Running,
        "the forker pid (this test process) is alive"
    );
    assert!(
        old.suspicious,
        "implausibly old for a still-\"running\" instance — likely pid recycling"
    );
}

#[test]
fn ls_does_not_list_partial_or_discard_tombstones() {
    let root = TempRoot::new("ls-tombstones");
    std::fs::create_dir_all(instances_root(root.path()).join("x.partial")).unwrap();
    std::fs::create_dir_all(instances_root(root.path()).join("x.discard")).unwrap();
    assert!(list_instances_at(root.path()).unwrap().is_empty());
}

#[test]
fn rm_refuses_live_run_without_force_and_force_removes_it() {
    let root = TempRoot::new("rm-live");
    fork_instance_at(root.path(), "live", Path::new("/ws"), &[]).unwrap();
    let err = rm_at(root.path(), "live", false).unwrap_err();
    assert!(err.to_string().contains("--force"));
    assert!(instances_root(root.path()).join("live").exists());

    rm_at(root.path(), "live", true).unwrap();
    assert!(!instances_root(root.path()).join("live").exists());
}

#[test]
fn rm_removes_finished_run_without_force() {
    let root = TempRoot::new("rm-finished");
    fork_instance_at(root.path(), "done", Path::new("/ws"), &[]).unwrap();
    finish_run(&root, "done");
    rm_at(root.path(), "done", false).unwrap();
    assert!(!instances_root(root.path()).join("done").exists());
}

#[test]
fn rm_unknown_run_errors() {
    let root = TempRoot::new("rm-unknown");
    let err = rm_at(root.path(), "nope", false).unwrap_err();
    assert!(err.to_string().contains("no instance"));
}

#[test]
fn gc_sweeps_stale_discard_and_stale_partial_but_not_fresh_partial() {
    let root = TempRoot::new("gc-tombstones");
    // A `.discard` is always safe to sweep — by the time it exists,
    // `retire_instance` already fully drained the instance under the lock.
    std::fs::create_dir_all(instances_root(root.path()).join("x.discard")).unwrap();
    // A fresh `.partial` (no meta.toml yet) might be a fork in progress —
    // left alone; an old one is presumed abandoned.
    let fresh_partial = instances_root(root.path()).join("fresh.partial");
    std::fs::create_dir_all(&fresh_partial).unwrap();
    let stale_partial = instances_root(root.path()).join("stale.partial");
    std::fs::create_dir_all(&stale_partial).unwrap();
    set_mtime_seconds_ago(&stale_partial, STALE_PARTIAL_SECS + 60);

    let (partials, discards) = sweep_tombstones(root.path()).unwrap();
    assert_eq!(discards, 1);
    assert_eq!(partials, 1, "only the stale partial is swept");
    assert!(!instances_root(root.path()).join("x.discard").exists());
    assert!(!stale_partial.exists());
    assert!(
        fresh_partial.exists(),
        "a fresh partial might be an in-progress fork"
    );
}

#[test]
fn harvest_lazily_sweeps_tombstones_too() {
    // The lazy sweep folded into harvest_finished_at applies the same
    // judgment as gc, so crashes don't accumulate garbage between gc runs.
    let root = TempRoot::new("harvest-lazy-sweep");
    let stale_discard = instances_root(root.path()).join("x.discard");
    std::fs::create_dir_all(&stale_discard).unwrap();
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert!(!stale_discard.exists());
}

#[test]
fn gc_prunes_revisions_and_warns_about_stale_changesets_without_deleting() {
    let root = TempRoot::new("gc-full");
    root.write_base(".claude/.credentials.json", r#"{"token":"0"}"#);
    for i in 1..=3u32 {
        let run = format!("r{i}");
        let instance = fork_instance_at(root.path(), &run, Path::new("/ws"), &[]).unwrap();
        write(
            &instance.join(".claude/.credentials.json"),
            &format!(r#"{{"token":"{i}"}}"#),
        );
        finish_run(&root, &run);
        harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
    }
    assert_eq!(list_revision_dirs(root.path()).unwrap().len(), 3);

    stage_run(&root, "old-run", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
    backdate_changeset(&root, "old-run", 30);

    let report = gc_at(root.path(), 1, 14).unwrap();
    assert_eq!(report.revisions_pruned, 2, "pruned down to keep=1");
    assert_eq!(list_revision_dirs(root.path()).unwrap().len(), 1);
    assert_eq!(report.stale_changesets.len(), 1);
    assert_eq!(report.stale_changesets[0].run, "old-run");
    assert!(
        report.stale_changesets[0].age_days >= 30,
        "age reported in days"
    );
    assert!(
        inbox_root(root.path()).join("old-run").exists(),
        "gc must never delete a stale changeset, only warn"
    );
}

#[test]
fn gc_does_not_warn_about_fresh_changesets() {
    let root = TempRoot::new("gc-fresh");
    stage_run(&root, "fresh-run", |inst| {
        write(&inst.join(".claude/skills/x/SKILL.md"), "x");
    });
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
    let report = gc_at(root.path(), DEFAULT_REVISIONS_KEEP, 14).unwrap();
    assert!(report.stale_changesets.is_empty());
}

// ---------------------------------------------------------------------------
// Regression tests for issues found in review
// ---------------------------------------------------------------------------

#[test]
fn harvest_records_no_revision_when_the_classified_touch_writes_nothing() {
    // A run deletes a base secret it inherited at fork. Deletion is
    // deliberately never propagated (FR-10) — no base write actually
    // happens — even though the path is classified Secret and so is
    // pre-flagged (conservatively) as possibly touching the base. Recording
    // a revision here would be a phantom entry in `home log`/`diff` whose
    // snapshot is byte-identical to the current base.
    let root = TempRoot::new("phantom-revision");
    root.write_base(".claude/.credentials.json", r#"{"token":"v1"}"#);
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    std::fs::remove_file(instance.join(".claude/.credentials.json")).unwrap();
    finish_run(&root, "r");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"v1"}"#,
        "deletion is never propagated to the base (FR-10)"
    );
    assert!(
        list_revisions_at(root.path()).unwrap().is_empty(),
        "no base write actually happened, so no revision should be recorded"
    );
}

#[test]
fn pending_revision_guard_discards_on_drop_unless_disarmed() {
    let root = TempRoot::new("guard-drop");
    let partial = begin_revision_snapshot(root.path()).unwrap();
    assert!(partial.exists());
    {
        let _guard = PendingRevisionGuard(Some(partial.clone()));
        // Dropped here without being disarmed, simulating an early `?`
        // return from a fallible mutation loop.
    }
    assert!(
        !partial.exists(),
        "an un-disarmed guard discards its snapshot on drop, never leaking it"
    );
}

#[test]
fn pending_revision_guard_disarm_prevents_the_drop_discard() {
    let root = TempRoot::new("guard-disarm");
    let partial = begin_revision_snapshot(root.path()).unwrap();
    let guard = PendingRevisionGuard(Some(partial.clone()));
    let returned = guard.disarm();
    assert_eq!(returned, Some(partial.clone()));
    assert!(
        partial.exists(),
        "disarming hands ownership back without discarding"
    );
    discard_revision_snapshot(&partial); // manual cleanup
}

#[test]
fn gc_sweeps_orphaned_revision_snapshot_partials() {
    // Simulates a `kill -9` between `begin_revision_snapshot` and
    // `finalize_revision`/`discard_revision_snapshot`: the in-process
    // `PendingRevisionGuard` can't help here (Drop never runs on SIGKILL),
    // so `gc` is the actual crash-safety net for this window.
    let root = TempRoot::new("gc-revision-partial");
    let stray = begin_revision_snapshot(root.path()).unwrap();
    assert!(stray.exists());
    let report = gc_at(root.path(), DEFAULT_REVISIONS_KEEP, 14).unwrap();
    assert_eq!(report.removed_revision_snapshots, 1);
    assert!(!stray.exists());
}

#[test]
fn promote_clean_no_op_does_not_record_a_phantom_revision() {
    // Base independently advances to exactly the run's staged version
    // before promote runs: merge_entry reports a clean landing (nothing to
    // resolve, the path is consumed from the changeset) but performs no
    // actual write. Recording a revision here would snapshot a mutation
    // that never happened, and would evict a real older revision from a
    // bounded `revisions_keep` for nothing.
    let root = TempRoot::new("promote-noop-revision");
    root.write_base("CLAUDE.md", "v1\n");
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join("CLAUDE.md"), "v2\n");
    });
    root.write_base("CLAUDE.md", "v2\n"); // base already matches by promote time
    let outcome = promote_at(root.path(), "r", &[], &[], DEFAULT_REVISIONS_KEEP).unwrap();
    assert_eq!(
        outcome.promoted,
        vec!["CLAUDE.md"],
        "the path is still consumed from the changeset"
    );
    assert!(outcome.conflicts.is_empty());
    assert!(
        list_revisions_at(root.path()).unwrap().is_empty(),
        "a promote that writes nothing must not record a revision"
    );
}

#[test]
fn harvest_records_partial_progress_and_leaves_instance_for_retry_on_mid_loop_error() {
    // Two paths change in one run: a Secret (".aws/credentials", sorted
    // before ".claude.json") and a State path with invalid JSON that fails
    // to merge. The secret write must land AND be recorded as a revision
    // even though the harvest as a whole fails, and the instance must NOT
    // be retired, so a later harvest can retry it.
    let root = TempRoot::new("harvest-partial-failure");
    root.write_base(".aws/credentials", "old");
    root.write_base(".claude.json", r#"{"n":1}"#);
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    write(&instance.join(".aws/credentials"), "new");
    write(&instance.join(".claude.json"), "{not valid json");
    finish_run(&root, "r");

    let err = harvest_instance(
        root.path(),
        &instances_root(root.path()).join("r"),
        &[],
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap_err();
    assert!(err.to_string().contains("merging state"));

    assert_eq!(
        root.read_base(".aws/credentials").unwrap(),
        "new",
        "the secret write that succeeded before the failure still landed"
    );
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(
        revs.len(),
        1,
        "the landed write is recorded as a revision despite the overall failure"
    );
    assert_eq!(revs[0].paths, 1, "only the path that actually landed");

    assert!(
        instances_root(root.path()).join("r").exists(),
        "a failed harvest must not retire the instance -- it must stay retryable"
    );
}

#[test]
fn repair_base_swap_finishes_an_interrupted_publish() {
    // Simulates the crash point mid-`swap_base`: the live base has already
    // been retired to `.discard` and the new content published to
    // `.partial`, but the final `.partial` -> base rename never happened.
    let root = TempRoot::new("repair-partial");
    root.write_base("marker", "old-content");
    let base = root.base();
    let partial = sibling_suffix(&base, ".partial");
    std::fs::create_dir_all(&partial).unwrap();
    write_atomic(&partial.join("marker"), b"new-content").unwrap();
    let discard = sibling_suffix(&base, ".discard");
    std::fs::rename(&base, &discard).unwrap();
    assert!(!base.exists());

    repair_base_swap(root.path()).unwrap();

    assert!(base.exists(), "the interrupted publish is finished");
    assert_eq!(
        std::fs::read_to_string(base.join("marker")).unwrap(),
        "new-content"
    );
}

#[test]
fn repair_base_swap_restores_from_tombstone_if_no_partial_exists() {
    // Defensive case: only the discard tombstone survived (the partial
    // should normally exist too at this crash point) -- repair must not
    // leave the base permanently missing either way.
    let root = TempRoot::new("repair-discard-only");
    root.write_base("marker", "old-content");
    let base = root.base();
    let discard = sibling_suffix(&base, ".discard");
    std::fs::rename(&base, &discard).unwrap();
    assert!(!base.exists());

    repair_base_swap(root.path()).unwrap();

    assert!(
        base.exists(),
        "the base is restored rather than left missing"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("marker")).unwrap(),
        "old-content"
    );
}

#[test]
fn lock_base_repairs_an_interrupted_swap_automatically() {
    // The point of running repair at the top of every `lock_base`: any
    // base-touching operation self-heals an interrupted swap with no
    // separate repair step required.
    let root = TempRoot::new("repair-via-lock");
    root.write_base("marker", "old-content");
    let base = root.base();
    let partial = sibling_suffix(&base, ".partial");
    std::fs::create_dir_all(&partial).unwrap();
    write_atomic(&partial.join("marker"), b"new-content").unwrap();
    let discard = sibling_suffix(&base, ".discard");
    std::fs::rename(&base, &discard).unwrap();

    let _lock = lock_base(root.path()).unwrap();
    assert!(base.exists());
    assert_eq!(
        std::fs::read_to_string(base.join("marker")).unwrap(),
        "new-content"
    );
}

// ---------------------------------------------------------------------------
// Regression tests: lead review round 1
// ---------------------------------------------------------------------------

#[test]
fn diff_redacts_a_secret_declared_by_the_recorded_policy_even_with_different_current_policy() {
    // A project declares `.config/mytool/token` secret via [[home.policy]]
    // (the documented pattern for credentials the built-in defaults don't
    // cover). The revision recording that harvest stores the policy that
    // was ACTIVE then; `diff` called later from a cwd whose policy doesn't
    // know about this rule at all must still redact it -- revisions are
    // global but policy is per-project.
    let overrides_at_record_time = vec![PolicyRule {
        glob: ".config/mytool/token".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];
    let root = TempRoot::new("diff-secret-recorded-policy");
    root.write_base(".config/mytool/token", "old-secret");
    // Forked WITH the same policy it's harvested under (this test is about
    // diff redaction across cwds at *diff* time, not the separate
    // cross-project fork/harvest pinning — see
    // `harvest_classifies_with_the_forking_projects_policy_not_the_harvesting_cwds`
    // for that).
    let instance = fork_instance_at(
        root.path(),
        "r",
        Path::new("/ws"),
        &overrides_at_record_time,
    )
    .unwrap();
    write(&instance.join(".config/mytool/token"), "new-secret");
    finish_run(&root, "r");
    harvest_finished_at(
        root.path(),
        &overrides_at_record_time,
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();

    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    // Diffed with an EMPTY current policy -- doesn't know this rule.
    let out = diff_at(root.path(), revs[0].seq, &[]).unwrap();
    assert!(out.contains(".config/mytool/token"));
    assert!(out.contains("secret — content not shown"));
    assert!(!out.contains("old-secret"));
    assert!(!out.contains("new-secret"));
}

#[test]
fn diff_redacts_a_path_the_current_policy_declares_secret_even_if_recorded_policy_didnt() {
    // At harvest time `.claude.json` was classified State by the built-in
    // defaults (not secret). A `[[home.policy]]` rule added AFTER the fact
    // making it secret must still protect this OLD revision's diff --
    // editing policy to protect a path re-protects old snapshots too.
    let root = TempRoot::new("diff-secret-current-policy");
    root.write_base(".claude.json", r#"{"n":1}"#);
    run_and_harvest(&root, "r", |inst| {
        write(&inst.join(".claude.json"), r#"{"n":2}"#);
    });
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);

    let current_overrides = vec![PolicyRule {
        glob: ".claude.json".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];
    let out = diff_at(root.path(), revs[0].seq, &current_overrides).unwrap();
    assert!(out.contains(".claude.json"));
    assert!(out.contains("secret — content not shown"));
    assert!(!out.contains("\"n\":1"));
    assert!(!out.contains("\"n\":2"));
}

#[test]
fn harvest_classifies_with_the_forking_projects_policy_not_the_harvesting_cwds() {
    // Project X declares `.config/mytool/token` secret via policy at FORK
    // time (pinned into InstanceMeta). A later harvest invoked with a
    // DIFFERENT (here: empty) policy -- as if it ran from project Y's cwd,
    // or a bare `pall8t home harvest`/`pall8t run` with no matching rule
    // (harvest is lazy, FR-8, so this is the common case, not an edge case)
    // -- must still classify and protect it as X's policy said: written
    // back to the base as a secret (FR-10), never staged into the inbox in
    // cleartext, and the harvest revision's own diff still redacts it even
    // when diffed with an empty CURRENT policy, because the policy pinned
    // into that revision's meta.toml also came from the fork-time policy.
    let policy_x = vec![PolicyRule {
        glob: ".config/mytool/token".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];
    let root = TempRoot::new("cross-project-policy");
    root.write_base(".config/mytool/token", "old-token");
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &policy_x).unwrap();
    write(&instance.join(".config/mytool/token"), "new-token");
    finish_run(&root, "r");

    // Harvested with an EMPTY policy -- the fork-time policy must win.
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();

    assert_eq!(
        root.read_base(".config/mytool/token").unwrap(),
        "new-token",
        "classified secret under the fork-time policy, written back per FR-10"
    );
    assert!(
        list_changesets_at(root.path()).unwrap().is_empty(),
        "must not be staged into the inbox in cleartext as knowledge"
    );

    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(revs.len(), 1);
    let out = diff_at(root.path(), revs[0].seq, &[]).unwrap();
    assert!(out.contains(".config/mytool/token"));
    assert!(out.contains("secret — content not shown"));
    assert!(!out.contains("old-token"));
    assert!(!out.contains("new-token"));
}

#[test]
fn harvest_uses_forks_authoritative_empty_policy_not_the_harvesting_cwds_override() {
    // The instance was forked with EXPLICITLY ZERO overrides
    // (`InstanceMeta::policy = Some(vec![])`) -- that's authoritative, not
    // "nothing recorded, fall back to the caller". A harvesting cwd whose
    // OWN policy reclassifies this path as ephemeral must not silently
    // discard it: the fork's own (empty) policy, which classifies an
    // unmatched path Knowledge by the conservative default, wins.
    let root = TempRoot::new("fork-empty-policy-authoritative");
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    write(&instance.join(".config/mytool/notes"), "some notes");
    finish_run(&root, "r");

    let harvesting_cwd_policy = vec![PolicyRule {
        glob: ".config/mytool/notes".to_string(),
        class: Some(Class::Ephemeral),
        strategy: None,
    }];
    harvest_finished_at(root.path(), &harvesting_cwd_policy, DEFAULT_REVISIONS_KEEP).unwrap();

    let changesets = list_changesets_at(root.path()).unwrap();
    assert_eq!(
        changesets.len(),
        1,
        "still staged as knowledge -- the harvesting cwd's ephemeral override must not apply"
    );
    assert_eq!(changesets[0].entries, 1);
}

#[test]
fn harvest_falls_back_to_caller_overrides_for_a_pre_field_instance_meta() {
    // Simulates an instance forked before `InstanceMeta::policy` existed:
    // meta.toml has no `policy` key at all, which must parse as `None` (via
    // `#[serde(default)]`) -- NOT `Some(vec![])` -- so harvest still falls
    // back to the caller's overrides, preserving pre-this-field behavior
    // for old instances rather than silently classifying them with an
    // empty (and wrongly authoritative) policy.
    let root = TempRoot::new("fork-pre-field-meta");
    let instance = fork_instance_at(root.path(), "r", Path::new("/ws"), &[]).unwrap();
    write(&instance.join(".config/mytool/token"), "a-secret");
    finish_run(&root, "r");

    let meta_path = instances_root(root.path()).join("r").join("meta.toml");
    let text = std::fs::read_to_string(&meta_path).unwrap();
    let patched: String = text
        .lines()
        .filter(|l| !l.starts_with("policy"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&meta_path, patched).unwrap();

    let caller_overrides = vec![PolicyRule {
        glob: ".config/mytool/token".to_string(),
        class: Some(Class::Secret),
        strategy: None,
    }];
    harvest_finished_at(root.path(), &caller_overrides, DEFAULT_REVISIONS_KEEP).unwrap();

    assert_eq!(
        root.read_base(".config/mytool/token").unwrap(),
        "a-secret",
        "a pre-field instance (no recorded policy at all) falls back to the caller's overrides"
    );
}

#[test]
fn parallel_runs_writing_identical_secret_value_records_only_one_revision() {
    // Both runs refresh the credential to the SAME new value. The second
    // harvest's write is byte-identical to what the first harvest already
    // wrote -- it must not count as a mutation and burn a second revision.
    let root = TempRoot::new("identical-secret-writes");
    root.write_base(".claude/.credentials.json", r#"{"token":"old"}"#);
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa"), &[]).unwrap();
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb"), &[]).unwrap();
    write(
        &inst_a.join(".claude/.credentials.json"),
        r#"{"token":"new"}"#,
    );
    write(
        &inst_b.join(".claude/.credentials.json"),
        r#"{"token":"new"}"#,
    );
    finish_run(&root, "run-a");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &[], DEFAULT_REVISIONS_KEEP).unwrap();

    assert_eq!(
        root.read_base(".claude/.credentials.json").unwrap(),
        r#"{"token":"new"}"#
    );
    let revs = list_revisions_at(root.path()).unwrap();
    assert_eq!(
        revs.len(),
        1,
        "the second run's write is byte-identical to the base after the first -- no new revision"
    );
}

#[test]
fn promote_union_merge_identical_to_current_consumes_path_without_recording_revision() {
    // Constructed so run-b's promote actually REACHES `write_or_conflict`'s
    // union path (unlike an earlier version of this test, which had both
    // runs stage an identical append -- that hit the pre-existing `c == t`
    // fast path in `merge_entry` before ever reaching the union branch, so
    // it stayed green even with the `write_if_changed` fix in
    // `write_or_conflict` reverted).
    //
    // notes.log starts as an EXISTING EMPTY file (ancestor = Some("")).
    // run-a stages ancestor="", theirs="l1\nl2\n"; run-b stages
    // ancestor="", theirs="l2\n". Promoting run-a fast-forwards (current
    // still equals ancestor) to "l1\nl2\n". Promoting run-b then hits
    // `current == "l1\nl2\n"`, which is neither `ancestor` nor `theirs`, so
    // it falls through to the union branch: `union_merge("", "l1\nl2\n",
    // "l2\n")` reconstructs exactly "l1\nl2\n" (verified against the real
    // `git merge-file --union` this shells out to) -- a merge that runs and
    // produces a result equal to `current`, which must not count as a
    // write.
    let overrides = vec![PolicyRule {
        glob: "notes.log".to_string(),
        class: Some(Class::Knowledge),
        strategy: Some(MergeStrategy::Union),
    }];
    let root = TempRoot::new("union-noop-revision");
    root.write_base("notes.log", "");

    // Both forked WITH the override in effect (fork-time policy is
    // authoritative at harvest — see `InstanceMeta::policy`).
    let inst_a = fork_instance_at(root.path(), "run-a", Path::new("/wa"), &overrides).unwrap();
    write(&inst_a.join("notes.log"), "l1\nl2\n");
    finish_run(&root, "run-a");
    let inst_b = fork_instance_at(root.path(), "run-b", Path::new("/wb"), &overrides).unwrap();
    write(&inst_b.join("notes.log"), "l2\n");
    finish_run(&root, "run-b");
    harvest_finished_at(root.path(), &overrides, DEFAULT_REVISIONS_KEEP).unwrap();

    let outcome_a = promote_at(
        root.path(),
        "run-a",
        &[],
        &overrides,
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();
    assert_eq!(outcome_a.promoted, vec!["notes.log".to_string()]);
    assert_eq!(root.read_base("notes.log").unwrap(), "l1\nl2\n");
    let revs_after_a = list_revisions_at(root.path()).unwrap().len();
    assert_eq!(revs_after_a, 1);

    let outcome_b = promote_at(
        root.path(),
        "run-b",
        &[],
        &overrides,
        DEFAULT_REVISIONS_KEEP,
    )
    .unwrap();
    assert_eq!(
        outcome_b.promoted,
        vec!["notes.log".to_string()],
        "the path is still consumed from run-b's changeset"
    );
    assert_eq!(
        root.read_base("notes.log").unwrap(),
        "l1\nl2\n",
        "the union merge reconstructs exactly what's already there"
    );
    let revs_after_b = list_revisions_at(root.path()).unwrap().len();
    assert_eq!(
        revs_after_b, revs_after_a,
        "no new revision for a union merge whose result is byte-identical to the base"
    );
}

#[test]
fn prune_revisions_clamps_zero_keep_to_one() {
    // revisions_keep = 0 must not prune the revision that was just written
    // (which would leave `home log` permanently empty and reset
    // `next_revision_seq` to 1 forever) -- it's clamped to keep the latest.
    let root = TempRoot::new("prune-zero-keep");
    root.write_base(".claude/.credentials.json", r#"{"token":"0"}"#);
    for i in 1..=3u32 {
        let run = format!("r{i}");
        let instance = fork_instance_at(root.path(), &run, Path::new("/ws"), &[]).unwrap();
        write(
            &instance.join(".claude/.credentials.json"),
            &format!(r#"{{"token":"{i}"}}"#),
        );
        finish_run(&root, &run);
        harvest_finished_at(root.path(), &[], 0).unwrap();
    }
    let dirs = list_revision_dirs(root.path()).unwrap();
    assert_eq!(
        dirs.len(),
        1,
        "revisions_keep=0 is clamped to 1, not 'erase everything including what was just written'"
    );
    assert_eq!(
        dirs[0].0, 3,
        "sequence numbers keep advancing rather than resetting to 1 each time"
    );
}
