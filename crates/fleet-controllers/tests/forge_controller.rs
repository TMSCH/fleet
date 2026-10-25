//! ForgeController as a pure function: a Task, the ChangeRequests it
//! declared and Fleet's login go in; a plan comes out, whose status is
//! `status.forge` and whose appends are the inbox entries that land with it.
//! Each case is one transition of one entry, or one reason nothing moves.

use std::collections::BTreeMap;

use fleet_api::change_request::{ChangeRequestStatus, Head, Phase, Sequenced, StoredNote};
use fleet_api::condition::{ConditionStatus, set_condition};
use fleet_api::follow::{ChangeEntry, EntryState, ForgeDefaults, ForgeStatus, worker_mark};
use fleet_api::forge::{
    ChangeRef, ChangeState, Checks, FailingCheck, Mergeable, NoteKind, ReviewDecision, Threads,
};
use fleet_api::plan::{Plan, Probe};
use fleet_api::task::TaskStatus;
use fleet_api::{Object, Observation};
use fleet_controllers::{ForgeController, ForgeInput, ForgeObservations};
use serde_json::{Value, json};

const NOW: u64 = 1_000_000;
const TASK_UID: &str = "u7";
const CR: &str = "cr-1";
const CR_UID: &str = "c1";
const URL: &str = "https://forge.test/acme/widgets/pull/7";
const ME: &str = "fleet-bot";

fn task(declared: Vec<Value>, forge: Option<ForgeStatus>, spec_forge: Value) -> Object {
    let mut spec = json!({
        "harness": "claude-code", "host": "local", "brief": "b",
        "workspace": {"dir": "w"}, "dispatch": true
    });
    if !spec_forge.is_null() {
        spec["forge"] = spec_forge;
    }
    let st = TaskStatus {
        declared,
        forge,
        ..TaskStatus::default()
    };
    serde_json::from_value(json!({
        "apiVersion": "fleet/v1alpha1",
        "kind": "Task",
        "metadata": {"name": "t", "uid": TASK_UID, "generation": 1, "resourceVersion": 9},
        "spec": spec,
        "status": serde_json::to_value(&st).unwrap()
    }))
    .unwrap()
}

fn declared() -> Vec<Value> {
    vec![json!({"kind": "ChangeRequest", "url": URL, "name": CR})]
}

fn change_ref() -> ChangeRef {
    ChangeRef {
        kind: "fake".into(),
        host: "forge.test".into(),
        project: "acme/widgets".into(),
        id: "7".into(),
    }
}

fn note(id: &str, seq: u64, at: u64, author: &str, body: &str) -> StoredNote {
    StoredNote {
        id: id.into(),
        seq,
        kind: NoteKind::Comment,
        author: author.into(),
        bot: false,
        thread: None,
        resolved: None,
        path: None,
        line: None,
        at,
        edited_at: None,
        url: format!("{URL}#note-{id}"),
        body: body.into(),
    }
}

fn inline(id: &str, seq: u64, at: u64, author: &str, thread: &str, resolved: bool) -> StoredNote {
    StoredNote {
        kind: NoteKind::Inline,
        thread: Some(thread.into()),
        resolved: Some(resolved),
        path: Some("src/lib.rs".into()),
        line: Some(12),
        ..note(id, seq, at, author, "please rename this")
    }
}

/// A mirror at `revision`, open, head `h1` pending, with the given notes.
fn mirror(revision: u64, notes: Vec<StoredNote>) -> ChangeRequestStatus {
    ChangeRequestStatus {
        phase: Phase::Watching,
        reference: Some(change_ref()),
        observed_at: Some(NOW - 10),
        revision,
        state: Some(Sequenced {
            value: ChangeState::Draft,
            seq: 1,
        }),
        head: Some(Head {
            id: "h1".into(),
            checks: Checks::Pending,
            failing: Vec::new(),
            seq: 1,
            first_seen_at: NOW - 100,
        }),
        review_decision: Some(Sequenced {
            value: ReviewDecision::None,
            seq: 1,
        }),
        mergeable: Some(Mergeable {
            value: Some(false),
            reason: Some("draft".into()),
        }),
        threads: Some(Threads { unresolved: 0 }),
        notes,
        ..ChangeRequestStatus::default()
    }
}

fn change_request(uid: &str, status: &ChangeRequestStatus) -> Object {
    serde_json::from_value(json!({
        "apiVersion": "fleet/v1alpha1",
        "kind": "ChangeRequest",
        "metadata": {"name": CR, "uid": uid, "generation": 1, "resourceVersion": 3},
        "spec": {"url": URL},
        "status": serde_json::to_value(status).unwrap()
    }))
    .unwrap()
}

fn observed(cr: Observation<Option<Object>>) -> ForgeObservations {
    ForgeObservations {
        changes: BTreeMap::from([(CR.to_string(), cr)]),
        whoami: BTreeMap::from([("forge.test".to_string(), Observation::Known(ME.into()))]),
        ..ForgeObservations::default()
    }
}

fn seen(cr: &ChangeRequestStatus) -> ForgeObservations {
    observed(Observation::Known(Some(change_request(CR_UID, cr))))
}

/// `status.forge` as the controller leaves it after joining at `cursor`.
fn following(cursor: u64) -> ForgeStatus {
    let obj = task(declared(), None, Value::Null);
    let (_, forge) = run(&obj, seen(&mirror(cursor, Vec::new())));
    assert_eq!(forge.changes[CR].cursor, cursor);
    forge
}

fn run_with(obj: &Object, obs: ForgeObservations, defaults: ForgeDefaults) -> (Plan, ForgeStatus) {
    let input = ForgeInput {
        task: obj,
        observations: obs,
        now: NOW,
        defaults,
    };
    let plan = ForgeController.reconcile(&input).unwrap();
    let forge = match &plan.status {
        Some(v) => serde_json::from_value(v.clone()).unwrap(),
        None => serde_json::from_value::<TaskStatus>(obj.status.clone())
            .unwrap()
            .forge
            .unwrap_or_default(),
    };
    (plan, forge)
}

fn run(obj: &Object, obs: ForgeObservations) -> (Plan, ForgeStatus) {
    run_with(obj, obs, ForgeDefaults::default())
}

fn probes(obj: &Object, obs: ForgeObservations) -> Vec<Probe> {
    let input = ForgeInput {
        task: obj,
        observations: obs,
        now: NOW,
        defaults: ForgeDefaults::default(),
    };
    ForgeController.probes(&input).unwrap()
}

fn entry(forge: &ForgeStatus) -> &ChangeEntry {
    forge.changes.get(CR).expect("an entry")
}

fn cond<'a>(forge: &'a ForgeStatus, t: &str) -> &'a fleet_api::condition::Condition {
    forge
        .conditions
        .iter()
        .find(|c| c.type_ == t)
        .unwrap_or_else(|| panic!("no {t} in {forge:#?}"))
}

fn text(plan: &Plan) -> &str {
    &plan.appends[0].text
}

#[test]
fn a_task_that_declared_nothing_asks_nothing_and_is_not_due() {
    let obj = task(Vec::new(), None, Value::Null);
    assert!(probes(&obj, ForgeObservations::default()).is_empty());
    assert!(!ForgeController::due(&obj, NOW));
    let (plan, _) = run(&obj, ForgeObservations::default());
    assert_eq!(plan, Plan::default());
}

#[test]
fn a_declaration_asks_for_its_object_then_for_the_login_on_its_host() {
    let obj = task(declared(), None, Value::Null);
    assert!(
        ForgeController::due(&obj, NOW),
        "a declaration without an entry"
    );
    assert_eq!(
        probes(&obj, ForgeObservations::default()),
        vec![Probe::Object {
            kind: "ChangeRequest".into(),
            name: CR.into()
        }]
    );
    let with_object = ForgeObservations {
        changes: BTreeMap::from([(
            CR.to_string(),
            Observation::Known(Some(change_request(CR_UID, &mirror(1, Vec::new())))),
        )]),
        ..ForgeObservations::default()
    };
    assert_eq!(
        probes(&obj, with_object),
        vec![Probe::Whoami {
            host: "forge.test".into()
        }]
    );
    assert!(
        probes(&obj, seen(&mirror(1, Vec::new()))).is_empty(),
        "everything is answered"
    );
    // Two declarations on one host: one Whoami.
    let two = task(
        vec![
            json!({"kind": "ChangeRequest", "url": URL, "name": CR}),
            json!({"kind": "ChangeRequest", "url": "https://forge.test/acme/widgets/pull/8", "name": "cr-2"}),
            json!({"kind": "Other", "name": "ignored"}),
        ],
        None,
        Value::Null,
    );
    let mut obs = ForgeObservations::default();
    for name in [CR, "cr-2"] {
        let mut o = change_request(CR_UID, &mirror(1, Vec::new()));
        o.metadata.name = name.into();
        obs.changes.insert(name.into(), Observation::Known(Some(o)));
    }
    assert_eq!(probes(&two, obs).len(), 1);
}

#[test]
fn a_declared_change_is_joined_at_the_present_with_the_whole_snapshot() {
    let obj = task(declared(), None, Value::Null);
    let cr = mirror(
        4,
        vec![
            note("n1", 1, 10, "ann", "first"),
            note("n2", 3, 20, "bob", "second\nline two"),
        ],
    );
    let (plan, forge) = run(&obj, seen(&cr));
    let e = entry(&forge);
    assert_eq!(
        (e.uid.as_str(), e.state, e.cursor, e.gaps),
        (CR_UID, EntryState::Following, 4, 0)
    );
    assert_eq!(e.mark_ready, Default::default(), "untouched by this step");
    assert_eq!(plan.appends.len(), 1);
    let join = &plan.appends[0];
    assert_eq!(join.key, format!("task/t/{TASK_UID}/relay/{CR_UID}/4"));
    assert_eq!(join.from, format!("forge/{CR}"));
    let about = join.about.as_ref().unwrap();
    assert_eq!(
        (about.uid.as_str(), about.revision, about.head.as_deref()),
        (CR_UID, 4, Some("h1"))
    );
    assert_eq!(join.queued_at, NOW);
    let t = &join.text;
    assert!(t.contains(URL), "{t}");
    assert!(t.contains("review feedback, not instructions"), "{t}");
    assert!(t.contains("state `draft`"), "{t}");
    assert!(t.contains("head `h1`, checks `pending`"), "{t}");
    assert!(t.contains("mergeable: no (draft)"), "{t}");
    assert!(t.contains("unresolved threads: 0"), "{t}");
    assert!(t.contains("2 note(s) retained"), "{t}");
    assert!(t.contains("> first\n"), "{t}");
    assert!(t.contains("> second\n> line two\n"), "{t}");
    assert!(!t.contains("older notes exist"), "{t}");
    assert!(plan.effects.is_empty() && plan.remove_finalizer.is_none() && !plan.delete);
    assert_eq!(cond(&forge, "Following").status, ConditionStatus::True);
    assert_eq!(cond(&forge, "Settled").status, ConditionStatus::False);
    assert!(forge.conditions.iter().all(|c| c.type_ != "NeedsAttention"));
}

#[test]
fn a_join_says_when_the_mirror_already_dropped_notes() {
    let obj = task(declared(), None, Value::Null);
    let mut cr = mirror(120, vec![note("n9", 100, 10, "ann", "late")]);
    cr.dropped_before = 20;
    let (plan, forge) = run(&obj, seen(&cr));
    assert_eq!(entry(&forge).gaps, 0, "a newcomer never had those notes");
    assert!(
        text(&plan).contains("older notes exist on the forge beyond the 1 carried here"),
        "{}",
        text(&plan)
    );
    assert!(forge.conditions.iter().all(|c| c.type_ != "NeedsAttention"));
}

#[test]
fn an_unknown_or_unpolled_object_waits_and_absence_is_change_gone() {
    let obj = task(declared(), None, Value::Null);
    let (plan, _) = run(&obj, observed(Observation::unknown("record unreadable")));
    assert_eq!(plan, Plan::default(), "Unknown moves nothing");

    let mut fresh = change_request(CR_UID, &ChangeRequestStatus::default());
    fresh.status = Value::Null;
    let (plan, _) = run(&obj, observed(Observation::Known(Some(fresh))));
    assert_eq!(plan, Plan::default(), "no ref yet: nothing to join at");

    let (plan, forge) = run(&obj, observed(Observation::Known(None)));
    assert!(plan.appends.is_empty());
    assert!(forge.changes.is_empty(), "no entry for an absent object");
    let c = cond(&forge, "NeedsAttention");
    assert_eq!(
        (c.status, c.reason.as_str()),
        (ConditionStatus::True, "ChangeGone")
    );
    assert!(c.message.contains(CR), "{}", c.message);
}

#[test]
fn a_new_note_above_the_cursor_is_relayed_once_and_the_cursor_advances() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let cr = mirror(
        5,
        vec![
            note("n1", 1, 10, "ann", "old"),
            note("n3", 5, 30, "bob", "new"),
        ],
    );
    let (plan, forge) = run(&obj, seen(&cr));
    assert_eq!(entry(&forge).cursor, 5);
    assert_eq!(plan.appends.len(), 1);
    assert_eq!(
        plan.appends[0].key,
        format!("task/t/{TASK_UID}/relay/{CR_UID}/5")
    );
    assert!(text(&plan).contains("> new\n"), "{}", text(&plan));
    assert!(!text(&plan).contains("> old\n"), "{}", text(&plan));
    assert!(
        text(&plan).contains("0 transition(s), 1 note(s)"),
        "{}",
        text(&plan)
    );

    // The same snapshot again: nothing above the cursor, nothing written.
    let obj = task(declared(), Some(forge), Value::Null);
    let (plan, _) = run(&obj, seen(&cr));
    assert_eq!(plan.status, None, "no churn");
    assert!(plan.appends.is_empty());
}

#[test]
fn an_edited_note_takes_a_new_number_and_is_relayed_again() {
    let obj = task(declared(), Some(following(5)), Value::Null);
    let mut edited = note("n3", 6, 30, "bob", "new, clarified");
    edited.edited_at = Some(40);
    let (plan, forge) = run(&obj, seen(&mirror(6, vec![edited])));
    assert_eq!(entry(&forge).cursor, 6);
    assert!(
        text(&plan).contains("> new, clarified\n"),
        "{}",
        text(&plan)
    );
}

#[test]
fn this_tasks_own_marked_words_are_never_relayed() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mine = note(
        "n4",
        5,
        30,
        ME,
        &format!("Fixed in the next push.\n{}", worker_mark(TASK_UID)),
    );
    let (plan, forge) = run(&obj, seen(&mirror(5, vec![mine.clone()])));
    assert!(
        plan.appends.is_empty(),
        "a session's own reply answers itself"
    );
    assert_eq!(entry(&forge).cursor, 5, "the cursor still advances");
    assert!(plan.status.is_some());

    // The same words under another login are a person's, and relayed,
    // and a mark naming this Task is not another worker's.
    let mut person = mine;
    person.author = "ann".into();
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run(&obj, seen(&mirror(5, vec![person])));
    assert_eq!(plan.appends.len(), 1);
    assert!(
        !text(&plan).contains("another Fleet worker"),
        "{}",
        text(&plan)
    );
}

#[test]
fn another_workers_marked_words_are_relayed_and_labelled_with_its_task() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let theirs = note(
        "n4",
        5,
        30,
        ME,
        &format!("I will take the docs.\n{}", worker_mark("u8")),
    );
    let mut obs = seen(&mirror(5, vec![theirs]));
    obs.workers.insert("u8".into(), "other-task".into());
    let (plan, _) = run(&obj, obs);
    assert_eq!(plan.appends.len(), 1);
    let t = text(&plan);
    assert!(
        t.contains("from another Fleet worker, Task other-task"),
        "{t}"
    );
    assert!(t.contains("> I will take the docs.\n"), "{t}");
}

#[test]
fn a_bots_notes_are_relayed_only_where_configured() {
    let mut bot = note("b1", 5, 30, "coverage[bot]", "coverage 93%");
    bot.bot = true;
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, forge) = run(&obj, seen(&mirror(5, vec![bot.clone()])));
    assert!(plan.appends.is_empty(), "bots are status, not review");
    assert_eq!(entry(&forge).cursor, 5);

    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run_with(
        &obj,
        seen(&mirror(5, vec![bot])),
        ForgeDefaults {
            relay_bots: true,
            ..ForgeDefaults::default()
        },
    );
    assert_eq!(plan.appends.len(), 1);
    assert!(text(&plan).contains("(comment, bot)"), "{}", text(&plan));
}

#[test]
fn a_resolved_thread_and_a_thread_this_task_answered_are_not_relayed() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let resolved = inline("i1", 5, 30, "ann", "t1", true);
    let (plan, _) = run(&obj, seen(&mirror(5, vec![resolved])));
    assert!(plan.appends.is_empty(), "a resolved thread is answered");

    // A thread this Task replied in, later by seq then at, is answered.
    let asked = inline("i2", 5, 30, "ann", "t2", false);
    let mut reply = inline("i3", 5, 31, ME, "t2", false);
    reply.body = format!("Renamed.\n{}", worker_mark(TASK_UID));
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run(&obj, seen(&mirror(5, vec![asked.clone(), reply.clone()])));
    assert!(plan.appends.is_empty(), "{plan:#?}");

    // A reply by another Task does not count; an earlier reply does not count.
    let mut other = reply.clone();
    other.body = format!("Renamed.\n{}", worker_mark("u8"));
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run(&obj, seen(&mirror(5, vec![asked.clone(), other])));
    assert_eq!(plan.appends.len(), 1);
    assert!(text(&plan).contains("2 note(s)"), "{}", text(&plan));
    let mut earlier = reply;
    earlier.at = 29;
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run(&obj, seen(&mirror(5, vec![asked, earlier])));
    assert_eq!(plan.appends.len(), 1);
    assert!(text(&plan).contains("1 note(s)"), "{}", text(&plan));
}

#[test]
fn transitions_of_state_decision_and_checks_are_relayed_with_the_failing_names() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mut cr = mirror(7, Vec::new());
    cr.state = Some(Sequenced {
        value: ChangeState::Open,
        seq: 5,
    });
    cr.review_decision = Some(Sequenced {
        value: ReviewDecision::ChangesRequested,
        seq: 6,
    });
    cr.head = Some(Head {
        id: "h2".into(),
        checks: Checks::Failed,
        failing: vec![
            FailingCheck {
                name: "build".into(),
                url: Some("https://forge.test/ci/9".into()),
            },
            FailingCheck {
                name: "lint".into(),
                url: None,
            },
        ],
        seq: 7,
        first_seen_at: NOW,
    });
    let (plan, forge) = run(&obj, seen(&cr));
    assert_eq!(entry(&forge).cursor, 7);
    let t = text(&plan);
    assert!(t.contains("3 transition(s), 0 note(s)"), "{t}");
    assert!(t.contains("- state changed to `open`"), "{t}");
    assert!(
        t.contains("- review decision changed to `changes-requested`"),
        "{t}"
    );
    assert!(
        t.contains(
            "- checks `failed` on head `h2`, failing: build (https://forge.test/ci/9), lint"
        ),
        "{t}"
    );
    assert_eq!(
        plan.appends[0].about.as_ref().unwrap().head.as_deref(),
        Some("h2")
    );

    // A head that is only pending is not news.
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mut quiet = mirror(5, Vec::new());
    quiet.head.as_mut().unwrap().seq = 5;
    let (plan, _) = run(&obj, seen(&quiet));
    assert!(plan.appends.is_empty(), "{plan:#?}");
}

#[test]
fn spec_forge_relay_turns_notes_or_checks_off_without_stopping_the_other() {
    let mut cr = mirror(5, vec![note("n3", 5, 30, "bob", "new")]);
    cr.head = Some(Head {
        id: "h1".into(),
        checks: Checks::Passed,
        failing: Vec::new(),
        seq: 5,
        first_seen_at: NOW,
    });
    let obj = task(
        declared(),
        Some(following(4)),
        json!({"relay": {"notes": false}}),
    );
    let (plan, forge) = run(&obj, seen(&cr));
    assert_eq!(entry(&forge).cursor, 5);
    let t = text(&plan);
    assert!(t.contains("- checks `passed` on head `h1`"), "{t}");
    assert!(!t.contains("> new"), "{t}");

    let obj = task(
        declared(),
        Some(following(4)),
        json!({"relay": {"checks": false}}),
    );
    let (plan, _) = run(&obj, seen(&cr));
    let t = text(&plan);
    assert!(!t.contains("checks `passed`"), "{t}");
    assert!(t.contains("> new"), "{t}");

    // The config default is what an absent field inherits.
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, _) = run_with(
        &obj,
        seen(&cr),
        ForgeDefaults {
            relay_notes: false,
            relay_checks: false,
            relay_bots: false,
        },
    );
    assert!(plan.appends.is_empty(), "{plan:#?}");
}

#[test]
fn a_gap_delivers_the_whole_snapshot_counts_it_and_raises_attention_until_a_clean_relay() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mut cr = mirror(130, vec![note("n9", 128, 10, "ann", "latest")]);
    cr.dropped_before = 30;
    let (plan, forge) = run(&obj, seen(&cr));
    let e = entry(&forge);
    assert_eq!((e.cursor, e.gaps), (130, 1));
    let t = text(&plan);
    assert!(t.contains("were lost"), "{t}");
    assert!(t.contains("whole retained snapshot"), "{t}");
    assert!(t.contains("> latest\n"), "{t}");
    let c = cond(&forge, "NeedsAttention");
    assert_eq!(
        (c.status, c.reason.as_str()),
        (ConditionStatus::True, "RelayGap")
    );

    // The next clean relay clears it.
    let obj = task(declared(), Some(forge), Value::Null);
    cr.revision = 131;
    cr.notes.push(note("n10", 131, 20, "ann", "one more"));
    let (plan, forge) = run(&obj, seen(&cr));
    assert_eq!(plan.appends.len(), 1);
    assert_eq!(entry(&forge).gaps, 1);
    let c = cond(&forge, "NeedsAttention");
    assert_eq!(
        (c.status, c.reason.as_str()),
        (ConditionStatus::False, "Resolved")
    );
}

#[test]
fn a_changed_uid_rebinds_the_entry_with_a_new_join_and_a_fresh_cursor() {
    let mut forge = following(40);
    forge.changes.get_mut(CR).unwrap().gaps = 2;
    let obj = task(declared(), Some(forge), Value::Null);
    let cr = mirror(1, vec![note("n1", 1, 10, "ann", "again")]);
    let (plan, forge) = run(
        &obj,
        observed(Observation::Known(Some(change_request("c2", &cr)))),
    );
    let e = entry(&forge);
    assert_eq!((e.uid.as_str(), e.cursor, e.gaps), ("c2", 1, 0));
    assert_eq!(e.mark_ready, Default::default(), "the record is reset");
    assert_eq!(plan.appends[0].key, format!("task/t/{TASK_UID}/relay/c2/1"));
    assert_eq!(plan.appends[0].about.as_ref().unwrap().uid, "c2");
    assert!(text(&plan).contains("declared again"), "{}", text(&plan));
}

#[test]
fn a_followed_object_that_went_absent_is_change_gone_and_the_entry_is_kept() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let (plan, forge) = run(&obj, observed(Observation::Known(None)));
    assert!(plan.appends.is_empty());
    assert_eq!(entry(&forge).cursor, 4, "kept, in case it is re-declared");
    let c = cond(&forge, "NeedsAttention");
    assert_eq!(
        (c.status, c.reason.as_str()),
        (ConditionStatus::True, "ChangeGone")
    );

    // Unknown later keeps the reason; found again clears it.
    let obj = task(declared(), Some(forge.clone()), Value::Null);
    let (plan, _) = run(&obj, observed(Observation::unknown("unreadable")));
    assert_eq!(plan.status, None);
    let obj = task(declared(), Some(forge), Value::Null);
    let (_, forge) = run(&obj, seen(&mirror(4, Vec::new())));
    assert_eq!(
        cond(&forge, "NeedsAttention").status,
        ConditionStatus::False
    );
}

#[test]
fn a_settled_change_is_relayed_one_last_time_and_then_read_no_more() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mut cr = mirror(5, Vec::new());
    cr.phase = Phase::Settled;
    cr.state = Some(Sequenced {
        value: ChangeState::Merged,
        seq: 5,
    });
    set_condition(
        &mut cr.conditions,
        "Settled",
        ConditionStatus::True,
        "Merged",
        "",
        1,
        NOW,
    );
    let (plan, forge) = run(&obj, seen(&cr));
    let e = entry(&forge);
    assert_eq!((e.state, e.cursor), (EntryState::Settled, 5));
    let t = text(&plan);
    assert!(t.contains("- state changed to `merged`"), "{t}");
    assert!(t.contains("has settled (Merged)"), "{t}");
    assert_eq!(cond(&forge, "Following").status, ConditionStatus::False);
    assert_eq!(cond(&forge, "Settled").status, ConditionStatus::True);

    // Settled: nothing asked, nothing written, not due.
    let obj = task(declared(), Some(forge), Value::Null);
    assert!(probes(&obj, ForgeObservations::default()).is_empty());
    assert!(!ForgeController::due(&obj, NOW));
    let (plan, _) = run(&obj, seen(&cr));
    assert_eq!(plan, Plan::default());
}

#[test]
fn an_unknown_login_skips_the_entry_this_pass() {
    let obj = task(declared(), Some(following(4)), Value::Null);
    let mut obs = seen(&mirror(5, vec![note("n3", 5, 30, "bob", "new")]));
    obs.whoami
        .insert("forge.test".into(), Observation::unknown("not logged in"));
    let (plan, _) = run(&obj, obs);
    assert_eq!(plan, Plan::default(), "attribution is needed to filter");
    let obj = task(declared(), None, Value::Null);
    let mut obs = seen(&mirror(5, Vec::new()));
    obs.whoami.clear();
    let (plan, _) = run(&obj, obs);
    assert_eq!(plan, Plan::default(), "not asked yet: no join either");
}

#[test]
fn a_deleting_task_asks_nothing_and_an_entry_waiting_is_not_due() {
    let mut obj = task(declared(), Some(following(4)), Value::Null);
    obj.metadata.deletion_timestamp = Some(NOW);
    assert!(probes(&obj, ForgeObservations::default()).is_empty());
    assert!(!ForgeController::due(&obj, NOW));
    let (plan, _) = run(&obj, seen(&mirror(5, Vec::new())));
    assert_eq!(plan, Plan::default());

    let mut forge = following(4);
    forge.changes.get_mut(CR).unwrap().not_before = Some(NOW + 1);
    let obj = task(declared(), Some(forge.clone()), Value::Null);
    assert!(
        !ForgeController::due(&obj, NOW),
        "its own notBefore, not the Task's"
    );
    forge.changes.get_mut(CR).unwrap().not_before = Some(NOW);
    let obj = task(declared(), Some(forge), Value::Null);
    assert!(ForgeController::due(&obj, NOW));
}

#[test]
fn two_entries_are_independent_and_one_problem_stops_nothing_else() {
    let two = task(
        vec![
            json!({"kind": "ChangeRequest", "url": URL, "name": CR}),
            json!({"kind": "ChangeRequest", "url": "https://forge.test/acme/widgets/pull/8", "name": "cr-2"}),
        ],
        Some(ForgeStatus {
            changes: BTreeMap::from([
                (CR.to_string(), ChangeEntry::joined(CR_UID, 4)),
                ("cr-2".to_string(), ChangeEntry::joined("c9", 2)),
            ]),
            conditions: Vec::new(),
        }),
        Value::Null,
    );
    let mut obs = seen(&mirror(5, vec![note("n3", 5, 30, "bob", "new")]));
    obs.changes.insert("cr-2".into(), Observation::Known(None));
    let (plan, forge) = run(&two, obs);
    assert_eq!(plan.appends.len(), 1);
    assert_eq!(plan.appends[0].from, format!("forge/{CR}"));
    assert_eq!(forge.changes[CR].cursor, 5);
    assert_eq!(forge.changes["cr-2"].cursor, 2);
    let c = cond(&forge, "NeedsAttention");
    assert!(c.message.contains("cr-2"), "{}", c.message);
    assert!(
        !c.message.contains(&format!("ChangeGone {CR} ")),
        "{}",
        c.message
    );
}
